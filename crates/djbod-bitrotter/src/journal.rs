use crate::storage::{directory, outside_devices, regular};
use anyhow::{ensure, Context, Result};
use rustix::fs::{flock, openat, FlockOperation, Mode, OFlags};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
struct Line<T> {
    at_unix_ms: u64,
    entry: T,
}

pub(crate) enum OpenMode {
    Create,
    Existing,
    CreateOrOpen,
}

pub(crate) struct Journal {
    file: File,
    pub entries: Vec<serde_json::Value>,
    poisoned: bool,
}

impl Journal {
    pub fn open(path: &Path, mode: OpenMode) -> Result<Self> {
        outside_devices(path)?;
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let parent = directory(absolute.parent().context("journal parent")?)?;
        let mut flags = OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match mode {
            OpenMode::Create => flags |= OFlags::CREATE | OFlags::EXCL,
            OpenMode::CreateOrOpen => flags |= OFlags::CREATE,
            OpenMode::Existing => {}
        }
        let mut file = File::from(openat(
            &parent,
            absolute.file_name().context("journal file name")?,
            flags,
            Mode::RUSR | Mode::WUSR,
        )?);
        regular(&file)?;
        flock(&file, FlockOperation::NonBlockingLockExclusive).context("journal already in use")?;
        parent.sync_all()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        // A torn final append cannot have authorized a subsequent mutation:
        // each intent is synced before writing a shard.
        if end < bytes.len() {
            file.set_len(end as u64)?;
            file.sync_all()?;
        }
        let mut entries = Vec::new();
        for line in bytes[..end]
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let value: Line<serde_json::Value> =
                serde_json::from_slice(line).context("invalid complete journal record")?;
            entries.push(value.entry);
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            entries,
            poisoned: false,
        })
    }
    pub fn read<T: DeserializeOwned>(&self) -> Result<Vec<T>> {
        self.entries
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }
    pub fn check_writable(&self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "journal write previously failed; restart and recover before continuing"
        );
        Ok(())
    }
    pub fn append<T: Serialize>(&mut self, entry: &T) -> Result<()> {
        self.check_writable()?;
        let value = serde_json::to_value(entry)?;
        let line = Line {
            at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            entry: &value,
        };
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        // Never append after a failed or partially written record in this
        // process. Recovery must inspect/truncate the tail first.
        self.poisoned = true;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        self.poisoned = false;
        self.entries.push(value);
        Ok(())
    }
}

pub fn save_plan(path: &Path, plan: &crate::model::Plan) -> Result<()> {
    plan.validate()?;
    let bytes = serde_json::to_vec_pretty(plan)?;
    ensure!(
        bytes.len() < crate::network::MAX_FRAME,
        "plan exceeds protocol frame limit"
    );
    outside_devices(path)?;
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = directory(path.parent().context("plan parent")?)?;
    let mut file = File::from(openat(
        &parent,
        path.file_name().context("plan name")?,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    parent.sync_all()?;
    Ok(())
}

pub fn load_plan(path: &Path) -> Result<crate::model::Plan> {
    outside_devices(path)?;
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = directory(path.parent().context("plan parent")?)?;
    let file = File::from(openat(
        &parent,
        path.file_name().context("plan name")?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    regular(&file)?;
    let mut bytes = Vec::new();
    file.take(crate::network::MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= crate::network::MAX_FRAME,
        "plan exceeds size limit"
    );
    let plan: crate::model::Plan = serde_json::from_slice(&bytes)?;
    plan.validate()?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_torn_tail_but_rejects_a_corrupt_complete_record() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("journal");
        let mut journal = Journal::open(&path, OpenMode::Create)?;
        journal.append(&serde_json::json!({"operation": "durable"}))?;
        let length = journal.file.metadata()?.len();
        assert!(
            Journal::open(&path, OpenMode::Existing).is_err(),
            "exclusive journal lock"
        );
        journal.file.write_all(b"{torn")?;
        drop(journal);
        let mut journal = Journal::open(&path, OpenMode::Existing)?;
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.file.metadata()?.len(), length);
        journal.file.write_all(b"{corrupt}\n")?;
        drop(journal);
        assert!(Journal::open(&path, OpenMode::Existing).is_err());
        Ok(())
    }

    #[test]
    fn io_failure_poisoning_requires_restart_before_another_operation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("journal");
        std::fs::write(&path, b"")?;
        let mut journal = Journal {
            file: File::open(&path)?,
            entries: Vec::new(),
            poisoned: false,
        };
        assert!(
            journal.append(&"intent").is_err(),
            "read-only descriptor simulates failed journal write"
        );
        assert!(journal.check_writable().is_err());
        journal.file = std::fs::OpenOptions::new().append(true).open(&path)?;
        assert!(journal.append(&"must not continue").is_err());
        assert!(std::fs::read(&path)?.is_empty());
        Ok(())
    }
}
