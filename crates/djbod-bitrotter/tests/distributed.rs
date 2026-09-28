mod common;

use anyhow::Result;
use common::{body, cli, consent, Cluster, BLOCK};
use djbod_bitrotter::{
    coordinator::{self, Coordinator, Limits, Selection},
    model::{self, Reply, Request},
    network::RpcClient,
    Stop,
};
use djbod_core::{
    checksum::checksum_block,
    erasure::ShardIndex,
    shardfile::{ShardFileReader, HEADER_LEN},
};
use djbod_proto::message::ScrubEvent;
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{symlink, FileExt},
};
use uuid::Uuid;

fn limits(events: u64) -> Limits {
    Limits {
        events: Some(events),
        interval_millis: 1,
        ..Limits::default()
    }
}

async fn findings(cluster: &Cluster) -> Result<usize> {
    let mut client = cluster.client().await?;
    let mut scrub = client.scrub(None, false).await?;
    let mut count = 0;
    loop {
        match scrub.next_event().await? {
            Ok(ScrubEvent::NodeFinding { .. } | ScrubEvent::ClusterFinding(_)) => count += 1,
            Ok(ScrubEvent::NodeFailed { .. } | ScrubEvent::CrossCheckStopped { .. }) => {
                panic!("scrub incomplete")
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(count)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_global_count_reconstructs_without_read_repair() -> Result<()> {
    let cluster = Cluster::new().await?;
    let original = body(3 * BLOCK as usize + 77);
    let record = cluster.put("test/target", &original).await?;
    for i in 0..12 {
        cluster
            .put(&format!("unrelated/{i}"), &body(91 + i))
            .await?;
    }
    let before = cluster.snapshot()?;
    let indices = cluster.different_nodes(&record);
    let plan = cluster.plan(&record.key, 2, indices.clone()).await?;
    assert_eq!(plan.workers.len(), 3);
    assert_eq!(plan.objects[0].selected, indices);
    assert_eq!(cluster.snapshot()?, before, "planning is read-only");
    assert_eq!(
        plan.id,
        cluster.plan(&record.key, 2, indices.clone()).await?.id
    );
    let selected_owners: BTreeSet<_> = indices
        .iter()
        .map(|i| {
            plan.owner(record.device_for(ShardIndex(*i)).unwrap())
                .unwrap()
        })
        .collect();
    assert_eq!(selected_owners.len(), 2);

    // Compare every block, including the shortened final block, with the
    // production reader. The fixture was written by ordinary product nodes.
    let coordinator = Coordinator::new(&plan)?;
    for stripe in 0..2 {
        for probe in coordinator.probe(&record, stripe).await? {
            let reader = ShardFileReader::open(&cluster.shard(&record, probe.index))?;
            let block = reader.read_block(stripe)?;
            assert_eq!(probe.actual_checksum, checksum_block(&block.bytes).0);
            assert_eq!(probe.stored_checksum, block.checksum.0);
            assert!(probe.intact);
            assert_eq!(block.bytes.len() as u64, reader.block_length(stripe)?);
        }
    }
    let journal = cluster.dir.path().join("run.jsonl");
    let mut reports = Vec::new();
    let summary = coordinator::run(
        &plan,
        consent(&plan),
        &journal,
        limits(1),
        false,
        Stop::default(),
        |r| reports.push(r.clone()),
    )
    .await?;
    assert_eq!((summary.completed, summary.mutations_reserved), (1, 2));
    assert_eq!(reports[0].bad_shards, indices);
    let after = cluster.snapshot()?;
    assert_eq!(after.len(), before.len());
    let mut changed = 0;
    for (path, old) in &before {
        let new = &after[path];
        assert_eq!(old.len(), new.len());
        let differences: Vec<_> = old
            .iter()
            .zip(new)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .collect();
        if differences.is_empty() {
            continue;
        }
        changed += 1;
        assert_eq!(differences.len(), 1);
        let (offset, (a, b)) = differences[0];
        assert_eq!((a ^ b).count_ones(), 1);
        let reader = ShardFileReader::open(path)?;
        assert!(indices.contains(&reader.header().shard_index.0));
        assert_eq!(reader.header().key_hash, record.key_hash);
        let begin = HEADER_LEN + reports[0].event.stripe * BLOCK;
        assert!(
            (begin..begin + reader.block_length(reports[0].event.stripe)?)
                .contains(&(offset as u64))
        );
    }
    assert_eq!(changed, 2);
    let mut client = cluster.client().await?;
    let (read, bytes) = client.get(&record.key).await?;
    assert_eq!(bytes, original);
    assert!(
        !read.reconstructed.is_empty(),
        "selected set includes data shard zero"
    );
    assert_eq!(cluster.snapshot()?, after, "GET does not repair on disk");
    assert!(findings(&cluster).await? >= 2);
    let repair = client.repair(&record.key).await?;
    assert_eq!(repair.shards.iter().filter(|s| s.rewritten).count(), 2);
    assert_eq!(findings(&cluster).await?, 0);
    assert!(coordinator
        .probe(&record, reports[0].event.stripe)
        .await?
        .iter()
        .all(|p| p.intact));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_requires_both_plan_bound_acknowledgements_above_tolerance() -> Result<()> {
    let cluster = Cluster::new().await?;
    let record = cluster.put("loss", &body(257)).await?;
    let plan = cluster.plan(&record.key, 3, vec![0, 1, 3]).await?;
    let file = cluster.dir.path().join("plan.json");
    let journal = cluster.dir.path().join("run.jsonl");
    coordinator::save_plan(&file, &plan)?;
    let before = cluster.snapshot()?;
    for acknowledgement in [None, Some("incorrect-plan"), Some(plan.id.as_str())] {
        let mut command = cli();
        command
            .args(["run", "--plan"])
            .arg(&file)
            .arg("--journal")
            .arg(&journal)
            .arg("--json");
        if let Some(id) = acknowledgement {
            command.args(["--confirm-test-damage", id]);
        }
        let output = command.output()?;
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr)?;
        assert!(error.contains("for testing purposes only"));
        assert!(error.contains("permanent data loss even when n <= m"));
        assert!(error.contains("certain data loss"));
        assert!(!journal.exists());
        assert_eq!(cluster.snapshot()?, before);
    }
    let output = cli()
        .args(["run", "--plan"])
        .arg(&file)
        .arg("--journal")
        .arg(&journal)
        .args([
            "--confirm-test-damage",
            &plan.id,
            "--confirm-data-loss",
            &plan.id,
            "--interval",
            "1ms",
            "--json",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(lines[0]["bad_shards"], serde_json::json!([0, 1, 3]));
    assert_eq!(lines.last().unwrap()["completed"], 1);
    let mut client = cluster.client().await?;
    assert!(
        client.get(&record.key).await.is_err(),
        "three damaged blocks exceed 3+2 tolerance"
    );
    assert!(client.repair(&record.key).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_plans_authentication_and_fencing_never_mutate() -> Result<()> {
    let cluster = Cluster::new().await?;
    let record = cluster.put("validation", &body(200)).await?;
    let before = cluster.snapshot()?;
    for (n, filter) in [
        (0, vec![]),
        (6, vec![]),
        (2, vec![1]),
        (2, vec![1, 1]),
        (2, vec![0, 5]),
    ] {
        assert!(cluster.plan(&record.key, n, filter).await.is_err());
    }
    let plan = cluster.plan(&record.key, 2, vec![]).await?;
    let mut missing = cluster.config.clone();
    missing.workers.pop();
    assert!(coordinator::create_plan(
        vec![cluster.bootstrap],
        missing,
        Selection {
            n: 2,
            keys: vec![record.key.clone()],
            ..Selection::default()
        }
    )
    .await
    .is_err());
    let mut duplicate = cluster.config.clone();
    duplicate.workers.push(duplicate.workers[0].clone());
    assert!(duplicate.validate().is_err());
    let mut altered = plan.clone();
    altered.n = 3;
    let rpc = RpcClient::new(&cluster.config.tls)?;
    let selected_node = plan.owner(
        record
            .device_for(ShardIndex(plan.objects[0].selected[0]))
            .unwrap(),
    )?;
    let endpoint = plan.endpoint(selected_node)?;
    assert!(rpc
        .request(
            endpoint,
            Request::Authorize {
                run: Uuid::new_v4(),
                consent: consent(&plan),
                plan: Box::new(altered)
            }
        )
        .await
        .is_err());
    let rogue = RpcClient::new(&cluster.rogue_tls)?;
    assert!(rogue.request(endpoint, Request::Describe).await.is_err());
    // A trusted server certificate alone is insufficient: mutual TLS must
    // reject clients that supply no certificate at all.
    let unauthenticated = async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(djbod_client::transport::read_authorities(
                &cluster.config.tls.ca,
            )?)
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(&endpoint.address).await?;
        let mut stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls))
            .connect(rustls::pki_types::ServerName::try_from("127.0.0.1")?, tcp)
            .await?;
        let request = br#"{"format":1,"request":{"operation":"describe"}}"#;
        stream.write_u32(request.len() as u32).await?;
        stream.write_all(request).await?;
        stream.read_u32().await?;
        Ok::<_, anyhow::Error>(())
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), unauthenticated)
            .await?
            .is_err()
    );
    let run = Uuid::new_v4();
    let authorize = || Request::Authorize {
        run,
        plan: Box::new(plan.clone()),
        consent: consent(&plan),
    };
    let Reply::Authorized { token: old } = rpc.request(endpoint, authorize()).await? else {
        panic!("authorization");
    };
    assert!(rpc
        .request(
            endpoint,
            Request::Authorize {
                run: Uuid::new_v4(),
                plan: Box::new(plan.clone()),
                consent: consent(&plan)
            }
        )
        .await
        .is_err());
    let Reply::Authorized { token: new } = rpc.request(endpoint, authorize()).await? else {
        panic!("authorization");
    };
    assert_ne!(new, old);
    assert!(rpc
        .request(
            endpoint,
            Request::Apply {
                run,
                token: old,
                sequence: 0
            }
        )
        .await
        .is_err());
    assert!(rpc
        .request(
            endpoint,
            Request::Apply {
                run,
                token: new,
                sequence: 0
            }
        )
        .await
        .is_err());
    assert!(coordinator::save_plan(
        &cluster.roots.values().next().unwrap().join("plan.json"),
        &plan
    )
    .is_err());
    assert_eq!(cluster.snapshot()?, before);

    // Versions in a mixed plan use their own erasure scheme, not today's
    // cluster defaults. These metadata-only checks need no file mutations.
    let mut low_parity = record.clone();
    low_parity.m = 1;
    low_parity.shards.truncate(4);
    assert_eq!(model::select(&low_parity, 2, 42, &[])?.len(), 2);
    let mut mixed = plan.clone();
    mixed.objects[0].record = low_parity;
    mixed.objects[0].record_checksum = mixed.objects[0].record.checksum().to_hex();
    mixed.objects[0].selected = vec![0, 1];
    mixed.seal()?;
    assert!(mixed.destructive());
    let mut unconfirmed = consent(&mixed);
    unconfirmed.data_loss = false;
    assert!(unconfirmed.validate(&mixed).is_err());
    mixed.n = 1;
    mixed.objects[0].record.m = 0;
    mixed.objects[0].record.shards.truncate(3);
    mixed.objects[0].record_checksum = mixed.objects[0].record.checksum().to_hex();
    mixed.objects[0].selected = vec![0];
    mixed.seal()?;
    assert!(mixed.destructive());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_and_coordinator_restarts_reconcile_without_replaying_a_bit() -> Result<()> {
    let mut cluster = Cluster::new().await?;
    let record = cluster.put("restart", &body(77)).await?;
    let plan = cluster
        .plan(&record.key, 2, cluster.different_nodes(&record))
        .await?;
    let journal = cluster.dir.path().join("run.jsonl");
    let first = coordinator::run(
        &plan,
        consent(&plan),
        &journal,
        limits(1),
        false,
        Stop::default(),
        |_| {},
    )
    .await?;
    let damaged = cluster.snapshot()?;

    // Simulate a coordinator crash after the durable apply decision and
    // worker writes, but before receipt/recording of acknowledgements.
    let lines: Vec<String> = fs::read_to_string(&journal)?
        .lines()
        .map(str::to_owned)
        .collect();
    let decision = lines
        .iter()
        .position(|l| l.contains("apply_decided"))
        .unwrap();
    fs::write(&journal, format!("{}\n", lines[..=decision].join("\n")))?;
    let rpc = RpcClient::new(&cluster.config.tls)?;
    let mut recovered_intent = false;
    for worker in &mut cluster.workers {
        worker.stop();
        let mut lines: Vec<String> = fs::read_to_string(&worker.config.journal)?
            .lines()
            .map(str::to_owned)
            .collect();
        if !recovered_intent && lines.last().unwrap().contains("\"phase\":\"applied\"") {
            lines.pop(); // Lost durable result: the intent and changed byte remain.
            fs::write(&worker.config.journal, format!("{}\n", lines.join("\n")))?;
            recovered_intent = true;
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&worker.config.journal)?
            .write_all(b"{torn-tail")?;
        worker.restart(&rpc).await?;
    }
    assert!(recovered_intent);
    let resumed = coordinator::run(
        &plan,
        consent(&plan),
        &journal,
        limits(1),
        true,
        Stop::default(),
        |_| {},
    )
    .await?;
    assert_eq!(resumed.run, first.run);
    assert_eq!((resumed.completed, resumed.mutations_reserved), (1, 2));
    assert_eq!(cluster.snapshot()?, damaged);
    let coordinator = Coordinator::new(&plan)?;
    let tokens = coordinator.authorize(first.run, &consent(&plan)).await?;
    coordinator
        .apply(first.run, &tokens, &plan.event(0)?)
        .await?;
    assert_eq!(cluster.snapshot()?, damaged, "duplicate apply is a no-op");

    cluster.client().await?.repair(&record.key).await?;
    let repaired = cluster.snapshot()?;
    coordinator
        .apply(first.run, &tokens, &plan.event(0)?)
        .await?;
    assert_eq!(
        cluster.snapshot()?,
        repaired,
        "retry cannot damage a repaired replacement"
    );

    cluster.workers[0].stop();
    fs::remove_file(&cluster.workers[0].config.journal)?;
    cluster.workers[0].restart(&rpc).await?;
    assert!(
        coordinator
            .authorize(first.run, &consent(&plan))
            .await
            .is_err(),
        "lost journal invalidates the old plan"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_partition_leaves_an_honest_partial_event_on_original_targets() -> Result<()> {
    let mut cluster = Cluster::new().await?;
    let record = cluster.put("partition", &body(97)).await?;
    let plan = cluster
        .plan(&record.key, 2, cluster.different_nodes(&record))
        .await?;
    let coordinator = Coordinator::new(&plan)?;
    let run = Uuid::new_v4();
    let tokens = coordinator.authorize(run, &consent(&plan)).await?;
    let event = plan.event(0)?;
    coordinator.prepare(run, &tokens, &event).await?;
    let selected = &plan.objects[0].selected;
    let offline_node = plan.owner(record.device_for(ShardIndex(selected[1])).unwrap())?;
    let offline = cluster
        .workers
        .iter()
        .position(|w| w.endpoint.node == offline_node)
        .unwrap();
    cluster.workers[offline].stop();
    let error = coordinator.apply(run, &tokens, &event).await.unwrap_err();
    assert!(error.to_string().contains("partial or uncertain"));
    let first_path = cluster.shard(&record, selected[0]);
    let first_bytes = fs::read(&first_path)?;
    let rpc = RpcClient::new(&cluster.config.tls)?;
    cluster.workers[offline].restart(&rpc).await?;
    let observed: Vec<_> = coordinator
        .probe(&record, 0)
        .await?
        .into_iter()
        .filter(|p| !p.intact)
        .map(|p| p.index)
        .collect();
    assert_eq!(observed, vec![selected[0]]);
    let tokens = coordinator.authorize(run, &consent(&plan)).await?;
    coordinator.apply(run, &tokens, &event).await?;
    assert_eq!(fs::read(first_path)?, first_bytes);
    let observed: Vec<_> = coordinator
        .probe(&record, 0)
        .await?
        .into_iter()
        .filter(|p| !p.intact)
        .map(|p| p.index)
        .collect();
    assert_eq!(observed, *selected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preexisting_damage_parity_scrub_and_mutation_limits() -> Result<()> {
    let cluster = Cluster::new().await?;
    let record = cluster.put("parity", &body(71)).await?;
    let plan = cluster.plan(&record.key, 2, vec![3, 4]).await?;
    let before = cluster.snapshot()?;
    let blocked = coordinator::run(
        &plan,
        consent(&plan),
        &cluster.dir.path().join("budget.jsonl"),
        Limits {
            max_mutations: Some(1),
            ..limits(1)
        },
        false,
        Stop::default(),
        |_| {},
    )
    .await?;
    assert_eq!(blocked.completed, 0);
    assert_eq!(cluster.snapshot()?, before);
    let mut reports = Vec::new();
    let summary = coordinator::run(
        &plan,
        consent(&plan),
        &cluster.dir.path().join("parity.jsonl"),
        limits(3),
        false,
        Stop::default(),
        |r| reports.push(r.clone()),
    )
    .await?;
    assert_eq!(
        (
            summary.completed,
            summary.skipped,
            summary.mutations_reserved
        ),
        (1, 2, 2)
    );
    assert_eq!(reports[1].status, "skipped_existing_damage");
    let mut client = cluster.client().await?;
    let (read, bytes) = client.get(&record.key).await?;
    assert_eq!(bytes, body(71));
    assert!(read.reconstructed.is_empty(), "GET need not read parity");
    assert!(findings(&cluster).await? >= 2);
    client.repair(&record.key).await?;
    assert_eq!(findings(&cluster).await?, 0);

    let stop = Stop::default();
    let mut events = Vec::new();
    let journal = cluster.dir.path().join("stop.jsonl");
    let stopped = coordinator::run(
        &plan,
        consent(&plan),
        &journal,
        limits(10),
        false,
        stop.clone(),
        |event| {
            events.push(event.clone());
            stop.stop();
        },
    )
    .await?;
    assert!(stopped.stopped);
    assert_eq!(stopped.completed, 1);
    let resumed = coordinator::run(
        &plan,
        consent(&plan),
        &journal,
        limits(10),
        true,
        Stop::default(),
        |_| {},
    )
    .await?;
    assert_eq!((resumed.completed, resumed.skipped), (1, 9));
    assert_eq!(resumed.mutations_reserved, 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_files_links_and_replacements_fail_closed() -> Result<()> {
    let cluster = Cluster::new().await?;
    let record = cluster.put("boundaries", &body(43)).await?;
    let plan = cluster.plan(&record.key, 1, vec![0]).await?;
    let coordinator = Coordinator::new(&plan)?;
    let path = cluster.shard(&record, 0);
    let original = fs::read(&path)?;
    for length in [0, 8, HEADER_LEN as usize - 1, original.len() - 1] {
        fs::write(&path, &original[..length])?;
        assert!(coordinator.probe(&record, 0).await.is_err());
    }
    fs::write(&path, &original)?;
    let mut oversized = original.clone();
    let length = oversized.len();
    oversized[length - 16..length - 8].copy_from_slice(&u64::MAX.to_le_bytes());
    fs::write(&path, &oversized)?;
    assert!(coordinator.probe(&record, 0).await.is_err());
    fs::write(&path, &original)?;
    let outside = cluster.dir.path().join("outside-shard");
    fs::rename(&path, &outside)?;
    symlink(&outside, &path)?;
    assert!(coordinator.probe(&record, 0).await.is_err());
    fs::remove_file(&path)?;
    fs::hard_link(&outside, &path)?;
    assert!(coordinator.probe(&record, 0).await.is_err());
    fs::remove_file(&path)?;
    fs::rename(&outside, &path)?;
    let run = Uuid::new_v4();
    let tokens = coordinator.authorize(run, &consent(&plan)).await?;
    coordinator.prepare(run, &tokens, &plan.event(0)?).await?;
    fs::write(&outside, &original)?;
    fs::rename(&outside, &path)?; // Same bytes, different inode after prepare.
    assert!(coordinator
        .apply(run, &tokens, &plan.event(0)?)
        .await
        .is_err());
    assert_eq!(fs::read(&path)?, original);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_placement_and_independent_bitrot_are_not_silently_retargeted() -> Result<()> {
    let cluster = Cluster::new().await?;
    let record = cluster.put("stale", &body(71)).await?;
    let plan = cluster.plan(&record.key, 2, vec![0, 1]).await?;
    let mut client = cluster.client().await?;
    client.move_shard(&record.key, 0, None).await?;
    let before = cluster.snapshot()?;
    assert!(coordinator::run(
        &plan,
        consent(&plan),
        &cluster.dir.path().join("stale.jsonl"),
        limits(1),
        false,
        Stop::default(),
        |_| {}
    )
    .await
    .is_err());
    assert_eq!(cluster.snapshot()?, before);

    // A fresh clean preflight cannot exclude unrelated damage arriving after
    // prepare. The exact-count check must not claim this as a successful n=2.
    let fresh = cluster.plan(&record.key, 2, vec![0, 1]).await?;
    // Restarting ends the failed run's short-lived lease, without clearing its audit trail.
    let rpc = RpcClient::new(&cluster.config.tls)?;
    let mut cluster = cluster;
    for worker in &mut cluster.workers {
        worker.restart(&rpc).await?;
    }
    let coordinator = Coordinator::new(&fresh)?;
    let run = Uuid::new_v4();
    let tokens = coordinator.authorize(run, &consent(&fresh)).await?;
    let event = fresh.event(0)?;
    coordinator.prepare(run, &tokens, &event).await?;
    let current = &fresh.objects[0].record;
    let path = cluster.shard(current, 2);
    let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0];
    file.read_exact_at(&mut byte, HEADER_LEN)?;
    byte[0] ^= 1;
    file.write_all_at(&byte, HEADER_LEN)?;
    file.sync_all()?;
    coordinator.apply(run, &tokens, &event).await?;
    assert_eq!(
        coordinator
            .probe(current, event.stripe)
            .await?
            .iter()
            .filter(|p| !p.intact)
            .count(),
        3
    );
    assert!(client.get(&record.key).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_prefix_plan_skips_empty_versions_and_covers_each_object() -> Result<()> {
    let cluster = Cluster::new().await?;
    for key in ["scope/a", "scope/b", "unrelated"] {
        cluster.put(key, &body(111)).await?;
    }
    cluster.put("scope/empty", &[]).await?;
    let before = cluster.snapshot()?;
    let workers = cluster.dir.path().join("workers.toml");
    fs::write(&workers, toml::to_string(&cluster.config)?)?;
    let path = cluster.dir.path().join("scope.json");
    let output = cli()
        .args([
            "plan",
            "--bootstrap-node",
            &cluster.bootstrap.to_string(),
            "--workers",
        ])
        .arg(&workers)
        .args([
            "--prefix", "scope/", "--shards", "2", "--seed", "42", "--out",
        ])
        .arg(&path)
        .arg("--json")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stderr)?.contains("skipped empty object: scope/empty"));
    let plan = coordinator::load_plan(&path)?;
    assert_eq!(
        plan.objects
            .iter()
            .map(|o| o.record.key.as_str())
            .collect::<Vec<_>>(),
        vec!["scope/a", "scope/b"]
    );
    assert_eq!(cluster.snapshot()?, before);
    // A copied manifest is still refused when placed inside a device tree.
    let unsafe_path = cluster.roots.values().next().unwrap().join("plan.json");
    fs::copy(&path, &unsafe_path)?;
    assert!(coordinator::load_plan(&unsafe_path).is_err());
    fs::remove_file(unsafe_path)?;
    let alias = cluster.dir.path().join("symlink-plan.json");
    symlink(&path, &alias)?;
    assert!(coordinator::load_plan(&alias).is_err());

    let mut reports = Vec::new();
    let summary = coordinator::run(
        &plan,
        consent(&plan),
        &cluster.dir.path().join("scope-run.jsonl"),
        limits(2),
        false,
        Stop::default(),
        |report| reports.push(report.clone()),
    )
    .await?;
    assert_eq!((summary.completed, summary.mutations_reserved), (2, 4));
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].bad_shards.len(), 2);
    assert_eq!(reports[1].bad_shards.len(), 2);
    assert_ne!(reports[0].key, reports[1].key);
    let (read, bytes) = cluster.client().await?.get("unrelated").await?;
    assert!(read.reconstructed.is_empty());
    assert_eq!(bytes, body(111));
    Ok(())
}
