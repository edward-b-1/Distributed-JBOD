//! `djbod-recover` (SPEC 20.2): given device directories and no running
//! cluster, list the versions present and reassemble any version for
//! which k intact shards can be found. Disk access uses `djbod-core`. It
//! reads the objects tree directly, so a device whose identity file is
//! lost is as good as any other, and it never writes to a device.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use xxhash_rust::xxh3::Xxh3;

use comfy_table::{presets::NOTHING, CellAlignment, Table};
use djbod_core::checksum::BlockChecksum;
use djbod_core::erasure::{ReedSolomonCode, ShardIndex};
use djbod_core::keyhash::{hash_key, KeyHash};
use djbod_core::layout::{
    parse_record_file_name, parse_shard_file_name, DEFAULT_BUCKET, OBJECTS_DIR,
};
use djbod_core::record::MetadataRecord;
use djbod_core::shardfile::{shard_geometry, ShardFileReader};
use djbod_core::stripe::{decode_stripe, DecodedStripe};
use djbod_core::version::VersionId;

#[derive(Parser)]
#[command(
    name = "djbod-recover",
    about = "Offline recovery from Distributed-JBOD device directories",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every key and version found on the given device paths, with
    /// how many of its shards are present and intact.
    List {
        #[arg(required = true)]
        device_paths: Vec<PathBuf>,
    },
    /// Reassemble one object from the shards found on the given device
    /// paths and write it to a file.
    Extract {
        key: String,
        /// The version to extract; the newest found if omitted.
        #[arg(long)]
        version: Option<String>,
        /// Where to write the object. Refused if the file exists.
        #[arg(long)]
        out: PathBuf,
        #[arg(required = true)]
        device_paths: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::List { device_paths } => list(&device_paths),
        Command::Extract {
            key,
            version,
            out,
            device_paths,
        } => extract(&key, version.as_deref(), &out, &device_paths),
    }
}

/// Everything found under the given device paths.
#[derive(Default)]
struct Found {
    /// The winning record for each version: the highest revision seen.
    records: BTreeMap<(String, VersionId), MetadataRecord>,
    /// Shard files by version and index, each opened and structurally
    /// sound.
    shards: BTreeMap<(KeyHash, VersionId), BTreeMap<ShardIndex, PathBuf>>,
    /// Damaged or misplaced files, reported and skipped.
    problems: Vec<String>,
}

/// Walk the objects tree of every path. Each device is a plain directory
/// here: only the layout of SPEC 9.3 is assumed.
fn scan(device_paths: &[PathBuf]) -> anyhow::Result<Found> {
    let mut found = Found::default();
    for root in device_paths {
        let bucket = root.join(OBJECTS_DIR).join(DEFAULT_BUCKET);
        if !bucket.is_dir() {
            found.problems.push(format!(
                "{}: no {OBJECTS_DIR}/{DEFAULT_BUCKET} directory; not a device or empty",
                root.display()
            ));
            continue;
        }
        for first in sorted_entries(&bucket)? {
            for second in sorted_entries(&first)? {
                for key_dir in sorted_entries(&second)? {
                    if key_dir.is_dir() {
                        scan_key_directory(&key_dir, &mut found)?;
                    }
                }
            }
        }
    }
    Ok(found)
}

fn scan_key_directory(key_dir: &Path, found: &mut Found) -> anyhow::Result<()> {
    let dir_name = key_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let Some(dir_hash) = KeyHash::from_hex(&dir_name) else {
        found.problems.push(format!(
            "{}: directory name is not a key hash; skipped",
            key_dir.display()
        ));
        return Ok(());
    };
    for entry in sorted_entries(key_dir)? {
        let name = entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if let Some(version) = parse_record_file_name(&name) {
            let json = match fs::read_to_string(&entry) {
                Ok(json) => json,
                Err(e) => {
                    found
                        .problems
                        .push(format!("{}: cannot read: {e}", entry.display()));
                    continue;
                }
            };
            let record = match MetadataRecord::from_json(&json) {
                Ok(record) => record,
                Err(e) => {
                    found
                        .problems
                        .push(format!("{}: damaged record: {e}", entry.display()));
                    continue;
                }
            };
            if record.key_hash != dir_hash || record.version != version {
                found.problems.push(format!(
                    "{}: record names another key or version; skipped",
                    entry.display()
                ));
                continue;
            }
            let slot = (record.key.clone(), record.version);
            match found.records.get(&slot) {
                Some(existing) if existing.revision > record.revision => {}
                Some(existing) if existing.revision == record.revision && *existing != record => {
                    found.problems.push(format!(
                        "{}: disagrees with another copy at the same revision; first copy kept",
                        entry.display()
                    ));
                }
                _ => {
                    found.records.insert(slot, record);
                }
            }
        } else if let Some((version, index)) = parse_shard_file_name(&name) {
            match ShardFileReader::open(&entry) {
                Ok(reader) => {
                    let header = reader.header();
                    if header.key_hash != dir_hash
                        || header.version_id != version
                        || header.shard_index != index
                    {
                        found.problems.push(format!(
                            "{}: shard header names another key, version, or index; skipped",
                            entry.display()
                        ));
                        continue;
                    }
                    found
                        .shards
                        .entry((dir_hash, version))
                        .or_default()
                        .insert(index, entry.clone());
                }
                Err(e) => found
                    .problems
                    .push(format!("{}: damaged shard file: {e}", entry.display())),
            }
        }
    }
    Ok(())
}

fn sorted_entries(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", dir.display()))?;
    entries.sort();
    Ok(entries)
}

/// Columns two spaces apart with no border, each as wide as its widest
/// cell, measured in terminal columns so wide characters line up.
/// `right_aligned` names the numeric columns, which align on their right
/// edge. Lines carry no trailing spaces.
fn render(mut table: Table, right_aligned: &[usize]) -> String {
    table.load_style(NOTHING);
    let last = table.column_count().saturating_sub(1);
    for (i, column) in table.column_iter_mut().enumerate() {
        column.set_padding((0, if i == last { 0 } else { 2 }));
        if right_aligned.contains(&i) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    let mut out = String::new();
    for line in table.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn list(device_paths: &[PathBuf]) -> anyhow::Result<ExitCode> {
    let found = scan(device_paths)?;
    let mut unrecoverable = 0usize;
    let mut table = Table::new();
    table.set_header(["KEY", "VERSION", "REV", "SIZE", "SHARDS", "STATUS"]);
    for ((key, version), record) in &found.records {
        let present = found
            .shards
            .get(&(record.key_hash, *version))
            .map(|s| s.len())
            .unwrap_or(0);
        let total = record.k as usize + record.m as usize;
        let status = if record.size == 0 || present >= record.k as usize {
            "recoverable"
        } else {
            unrecoverable += 1;
            "NOT recoverable"
        };
        table.add_row([
            key.clone(),
            version.to_text(),
            record.revision.to_string(),
            record.size.to_string(),
            format!("{present}/{total}"),
            status.to_string(),
        ]);
    }
    // Shards whose record was not found at all.
    for ((hash, version), shards) in &found.shards {
        if !found
            .records
            .values()
            .any(|r| r.key_hash == *hash && r.version == *version)
        {
            table.add_row([
                format!("(key hash {})", &hash.to_hex()[..16]),
                version.to_text(),
                "-".to_string(),
                "-".to_string(),
                format!("{}/?", shards.len()),
                "no record found; key unknown".to_string(),
            ]);
            unrecoverable += 1;
        }
    }
    print!("{}", render(table, &[2, 3]));
    for problem in &found.problems {
        eprintln!("problem: {problem}");
    }
    eprintln!(
        "{} version(s), {unrecoverable} not recoverable, {} problem(s)",
        found.records.len(),
        found.problems.len()
    );
    Ok(if unrecoverable > 0 || !found.problems.is_empty() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

fn extract(
    key: &str,
    version: Option<&str>,
    out: &Path,
    device_paths: &[PathBuf],
) -> anyhow::Result<ExitCode> {
    if out.exists() {
        bail!("{} exists; refusing to overwrite", out.display());
    }
    let found = scan(device_paths)?;
    for problem in &found.problems {
        eprintln!("problem: {problem}");
    }
    let key_hash = hash_key(key.as_bytes());
    let versions: Vec<&MetadataRecord> = found
        .records
        .iter()
        .filter(|((k, _), _)| k == key)
        .map(|(_, r)| r)
        .collect();
    if versions.is_empty() {
        bail!("no record of key {key:?} on the given paths");
    }
    let record = match version {
        Some(text) => {
            let wanted = VersionId::from_text(text)
                .with_context(|| format!("{text:?} is not a version id"))?;
            *versions
                .iter()
                .find(|r| r.version == wanted)
                .with_context(|| format!("no record of key {key:?} at version {text}"))?
        }
        None => versions.last().expect("non-empty"),
    };
    let scheme = record.scheme().context("record has an invalid scheme")?;
    eprintln!(
        "key {key:?} version {} revision {}: {} bytes, scheme {}+{}",
        record.version.to_text(),
        record.revision,
        record.size,
        record.k,
        record.m
    );

    let partial = out.with_extension(match out.extension() {
        Some(ext) => format!("{}.partial", ext.to_string_lossy()),
        None => "partial".to_string(),
    });
    let result = write_object(record, scheme, &found, &key_hash, &partial);
    match result {
        Ok(()) => {
            fs::rename(&partial, out)
                .with_context(|| format!("renaming {} to {}", partial.display(), out.display()))?;
            eprintln!("wrote {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Decode the object stripe by stripe into `partial`, verifying every
/// block and the whole-object checksum.
fn write_object(
    record: &MetadataRecord,
    scheme: djbod_core::erasure::Scheme,
    found: &Found,
    key_hash: &KeyHash,
    partial: &Path,
) -> anyhow::Result<()> {
    let mut output = BufWriter::new(
        fs::File::create(partial).with_context(|| format!("creating {}", partial.display()))?,
    );
    if record.size == 0 {
        output.flush()?;
        return Ok(());
    }
    let geometry = shard_geometry(scheme, record.block_size, record.size)
        .context("record has an impossible size")?;
    let empty = BTreeMap::new();
    let paths = found
        .shards
        .get(&(*key_hash, record.version))
        .unwrap_or(&empty);
    let mut readers: Vec<(ShardIndex, ShardFileReader)> = Vec::new();
    for (index, path) in paths {
        let reader = match ShardFileReader::open(path) {
            Ok(reader) => reader,
            Err(e) => {
                eprintln!("problem: {}: {e}; skipped", path.display());
                continue;
            }
        };
        let header = reader.header();
        let footer = reader.footer();
        if header.scheme != scheme
            || header.block_length != record.block_size
            || footer.object_size != record.size
            || footer.object_checksum != record.object_checksum
            || footer.block_count != geometry.block_count
        {
            eprintln!(
                "problem: {}: shard does not describe this version's object; skipped",
                path.display()
            );
            continue;
        }
        readers.push((*index, reader));
    }
    if readers.len() < scheme.data_shards() {
        bail!(
            "only {} intact shard(s) of {}+{} found; need at least {}",
            readers.len(),
            record.k,
            record.m,
            record.k
        );
    }
    eprintln!(
        "using shards {:?}",
        readers.iter().map(|(i, _)| i.0).collect::<Vec<u8>>()
    );

    let code = ReedSolomonCode::new(scheme);
    let all_indices = scheme.shard_indices();
    let stripe_size = scheme.data_shards() as u64 * record.block_size;
    let mut hasher = Xxh3::new();
    let mut repaired_stripes = 0u64;
    for stripe in 0..geometry.block_count {
        let mut received = Vec::with_capacity(readers.len());
        for (index, reader) in &readers {
            match reader.read_block(stripe) {
                Ok(block) => received.push(block),
                Err(e) => eprintln!(
                    "problem: shard {}: stripe {stripe} unreadable: {e}",
                    index.0
                ),
            }
        }
        let stripe_len = (record.size - stripe * stripe_size).min(stripe_size) as usize;
        let data = match decode_stripe(&code, &all_indices, &received, stripe_len)
            .context("decoding a stripe")?
        {
            DecodedStripe::Intact { data } => data,
            DecodedStripe::Repaired { data, faults } => {
                repaired_stripes += 1;
                eprintln!(
                    "stripe {stripe}: reconstructed around shard(s) {:?}",
                    faults.iter().map(|f| f.index.0).collect::<Vec<u8>>()
                );
                data
            }
            DecodedStripe::Unrecoverable {
                faults,
                usable,
                needed,
            } => bail!(
                "stripe {stripe}: only {usable} usable block(s) of {needed} needed; damaged shard(s) {:?}",
                faults.iter().map(|f| f.index.0).collect::<Vec<u8>>()
            ),
        };
        hasher.update(&data);
        output.write_all(&data)?;
    }
    output.flush()?;
    let computed = BlockChecksum(hasher.digest());
    if computed != record.object_checksum {
        bail!(
            "reassembled object has checksum {computed:?}, but the record says {:?}",
            record.object_checksum
        );
    }
    if repaired_stripes > 0 {
        eprintln!("{repaired_stripes} stripe(s) needed reconstruction");
    }
    Ok(())
}
