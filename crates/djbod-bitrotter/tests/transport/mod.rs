use super::{common, limits};
use anyhow::Result;
use common::{body, cli, consent, isolated_command, Cluster, WORKER_BINARY};
use djbod_bitrotter::{
    config::ClientTls,
    coordinator::{self, Coordinator},
    model::{Reply, Request},
    network::RpcClient,
    Stop,
};
use djbod_client::transport::TlsPaths;
use djbod_core::cluster::Transport;
use rcgen::{CertificateParams, KeyPair};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_modes_match_nodes_and_allowlists_cannot_be_bypassed() -> Result<()> {
    let mut cluster = Cluster::new().await?;
    let mutual = RpcClient::new(cluster.config.tls.as_ref())?;
    let anonymous = RpcClient::new(Some(&ClientTls {
        ca: cluster.config.tls.as_ref().unwrap().ca.clone(),
        cert: None,
        key: None,
    }))?;
    let unlisted = RpcClient::new(Some(&cluster.rogue_tls.clone().into()))?;
    let key = KeyPair::generate()?;
    let certificate = CertificateParams::new(Vec::<String>::new())?.self_signed(&key)?;
    let untrusted_cert = cluster.dir.path().join("untrusted.pem");
    let untrusted_key = cluster.dir.path().join("untrusted.key");
    fs::write(&untrusted_cert, certificate.pem())?;
    fs::write(&untrusted_key, key.serialize_pem())?;
    fs::set_permissions(&untrusted_key, fs::Permissions::from_mode(0o600))?;
    let untrusted = RpcClient::new(Some(&ClientTls {
        ca: cluster.config.tls.as_ref().unwrap().ca.clone(),
        cert: Some(untrusted_cert),
        key: Some(untrusted_key),
    }))?;
    let plain = RpcClient::new(None)?;
    let worker = &mut cluster.workers[0];
    let material = worker.config.tls.clone();
    let allowlist = worker.config.allowed_controllers.clone();
    for (mode, with_material, with_allowlist) in [
        (Transport::Plain, false, false),
        (Transport::Plain, true, false),
        (Transport::TlsOptional, true, false),
        (Transport::Tls, true, false),
        (Transport::Plain, true, true),
        (Transport::TlsOptional, true, true),
        (Transport::Tls, true, true),
    ] {
        worker.config.transport = mode;
        worker.config.tls = if with_material {
            material.clone()
        } else {
            None
        };
        worker.config.allowed_controllers = if with_allowlist {
            allowlist.clone()
        } else {
            Vec::new()
        };
        worker
            .restart(if with_material { &mutual } else { &plain })
            .await?;
        for (name, rpc, accepted) in [
            ("plain", &plain, mode != Transport::Tls && !with_allowlist),
            (
                "anonymous TLS",
                &anonymous,
                with_material && mode != Transport::Tls && !with_allowlist,
            ),
            ("mutual TLS", &mutual, with_material),
            (
                "unlisted certificate",
                &unlisted,
                with_material && !with_allowlist,
            ),
            ("untrusted certificate", &untrusted, false),
        ] {
            let reply = rpc.request(&worker.endpoint, Request::Describe).await;
            assert_eq!(
                matches!(reply, Ok(Reply::Description(_))),
                accepted,
                "{mode}, material={with_material}, allowlist={with_allowlist}, client={name}: {reply:?}"
            );
            if name == "plain" && mode == Transport::Tls {
                assert!(reply.unwrap_err().to_string().contains("TlsRequired"));
            }
        }
        if with_material {
            let mut wrong_name = worker.endpoint.clone();
            wrong_name.server_name = Some("wrong.example.invalid".into());
            assert!(mutual
                .request(&wrong_name, Request::Describe)
                .await
                .is_err());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plaintext_and_anonymous_tls_keep_confirmation_exact_counts_and_retry_safety() -> Result<()>
{
    for mode in [Transport::Plain, Transport::TlsOptional] {
        let mut cluster = Cluster::with_transport(mode).await?;
        if let Some(tls) = &mut cluster.config.tls {
            tls.cert = None;
            tls.key = None;
        }
        let original = body(257);
        let record = cluster.put("transport", &original).await?;
        let indices = cluster.different_nodes(&record);
        let before = cluster.snapshot()?;
        let workers = cluster.dir.path().join("workers.toml");
        fs::write(&workers, toml::to_string(&cluster.config)?)?;
        let path = cluster.dir.path().join("plan.json");
        let journal = cluster.dir.path().join("run.jsonl");
        let mut command = cli();
        command
            .env(
                "DJBOD_BOOTSTRAP_NODE",
                format!("{0},{0}", cluster.bootstrap),
            )
            .env("DJBOD_BITROTTER_WORKERS", &workers)
            .env(
                "DJBOD_CLUSTER",
                cluster.workers[0].config.cluster.to_string(),
            )
            .args(["plan", "--key", &record.key, "--shards", "2", "--out"])
            .arg(&path);
        for index in &indices {
            command.arg("--shard-index").arg(index.to_string());
        }
        let output = command.output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan = coordinator::load_plan(&path)?;
        assert_eq!(plan.config.tls, cluster.config.tls);
        assert_eq!(cluster.snapshot()?, before);

        let mut command = cli();
        command
            .args(["run", "--plan"])
            .arg(&path)
            .arg("--journal")
            .arg(&journal);
        let refusal = command.output()?;
        assert!(!refusal.status.success());
        let warning = String::from_utf8(refusal.stderr)?;
        assert!(warning.contains("for testing purposes only"));
        assert!(warning.contains("permanent data loss even when n <= m"));
        assert!(!journal.exists());
        assert_eq!(cluster.snapshot()?, before);

        // Transport and credentials are pinned in the plan. Even valid TLS
        // overrides cannot change the connection mode at execution time.
        let mut changed = cli();
        changed
            .args(["run", "--plan"])
            .arg(&path)
            .arg("--journal")
            .arg(&journal);
        client_flags(&mut changed, &cluster.rogue_tls);
        let refusal = changed.output()?;
        assert!(!refusal.status.success());
        assert!(
            String::from_utf8(refusal.stderr)?.contains("TLS options differ from the saved plan")
        );
        assert!(!journal.exists());
        assert_eq!(cluster.snapshot()?, before);

        if let Some(tls) = &cluster.config.tls {
            command.arg("--tls-ca").arg(&tls.ca);
        }
        let output = command
            .args(["--confirm-test-damage", &plan.id, "--json"])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let first: serde_json::Value = serde_json::from_str(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap(),
        )?;
        assert_eq!(first["bad_shards"], serde_json::json!(indices));
        let damaged = cluster.snapshot()?;
        assert_eq!(
            before
                .iter()
                .filter(|(path, bytes)| damaged[*path] != **bytes)
                .count(),
            2
        );
        assert_eq!(cluster.client().await?.get(&record.key).await?.1, original);

        let rpc = RpcClient::new(cluster.config.tls.as_ref())?;
        for worker in &mut cluster.workers {
            worker.restart(&rpc).await?;
        }
        let summary = coordinator::run(
            &plan,
            consent(&plan),
            &journal,
            limits(1),
            true,
            Stop::default(),
            |_| {},
        )
        .await?;
        assert_eq!((summary.completed, summary.mutations_reserved), (1, 2));
        let coordinator = Coordinator::new(&plan)?;
        let tokens = coordinator.authorize(summary.run, &consent(&plan)).await?;
        coordinator
            .apply(summary.run, &tokens, &plan.event(0)?)
            .await?;
        assert_eq!(
            cluster.snapshot()?,
            damaged,
            "anonymous retries must not repeat a bit flip"
        );
    }
    Ok(())
}

fn tls_environment(command: &mut Command, paths: &TlsPaths) {
    command
        .env("DJBOD_TLS_CA", &paths.ca)
        .env("DJBOD_TLS_CERT", &paths.cert)
        .env("DJBOD_TLS_KEY", &paths.key);
}

fn client_flags(command: &mut Command, paths: &TlsPaths) {
    command
        .arg("--tls-ca")
        .arg(&paths.ca)
        .arg("--tls-cert")
        .arg(&paths.cert)
        .arg("--tls-key")
        .arg(&paths.key);
}

fn missing_material() -> TlsPaths {
    TlsPaths {
        ca: "/no-such-bitrotter-authority.pem".into(),
        cert: "/no-such-bitrotter-certificate.pem".into(),
        key: "/no-such-bitrotter-key.pem".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn controller_tls_precedence_is_arguments_then_environment_then_file() -> Result<()> {
    let cluster = Cluster::with_transport(Transport::TlsOptional).await?;
    let record = cluster.put("precedence", &body(111)).await?;
    let tls = cluster.config.tls.as_ref().unwrap();
    let actual = TlsPaths {
        ca: tls.ca.clone(),
        cert: tls.cert.clone().unwrap(),
        key: tls.key.clone().unwrap(),
    };
    let workers = cluster.dir.path().join("workers.toml");
    let before = cluster.snapshot()?;
    for source in ["file", "environment", "arguments", "anonymous"] {
        let mut config = cluster.config.clone();
        if source != "file" {
            config.tls = Some(missing_material().into());
        }
        fs::write(&workers, toml::to_string(&config)?)?;
        let path = cluster.dir.path().join(format!("{source}.json"));
        let mut command = cli();
        if source == "environment" {
            tls_environment(&mut command, &actual);
        } else if source == "arguments" {
            tls_environment(&mut command, &missing_material());
            client_flags(&mut command, &actual); // Global options before the subcommand.
        } else if source == "anonymous" {
            command.arg("--tls-ca").arg(&actual.ca);
        }
        let output = command
            .args([
                "plan",
                "--bootstrap-node",
                &cluster.bootstrap.to_string(),
                "--workers",
            ])
            .arg(&workers)
            .args(["--key", &record.key, "--shards", "1", "--out"])
            .arg(&path)
            .output()?;
        assert!(
            output.status.success(),
            "{source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan = coordinator::load_plan(&path)?;
        let mut expected = tls.clone();
        if source == "anonymous" {
            expected.cert = None;
            expected.key = None;
        }
        assert_eq!(plan.config.tls.as_ref(), Some(&expected));
        assert!(
            plan.config.product_tls.is_none(),
            "worker credentials do not alter product transport"
        );
    }
    let output = cli()
        .arg("--tls-cert")
        .arg(&actual.cert)
        .arg("fingerprint")
        .arg(&actual.cert)
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr)?;
    assert!(error.contains("--tls-key") && error.contains("--tls-ca"));
    assert_eq!(cluster.snapshot()?, before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_configuration_precedence_and_environment_only_plaintext() -> Result<()> {
    let mut cluster = Cluster::new().await?;
    let rpc = RpcClient::new(cluster.config.tls.as_ref())?;
    let worker = &mut cluster.workers[0];
    let actual = worker.config.tls.clone().unwrap();
    worker.config.tls = Some(missing_material());
    worker.config.allowed_controllers.clear();
    worker
        .restart_with(&rpc, |command| tls_environment(command, &actual))
        .await?;
    let address = worker.config.listen.to_string();
    worker
        .restart_with(&rpc, |command| {
            tls_environment(command, &missing_material());
            client_flags(command, &actual);
            command
                .env("DJBOD_LISTEN", "127.0.0.1:0")
                .env("DJBOD_TRANSPORT", "tls-optional")
                .args(["--listen", &address, "--transport", "tls"]);
        })
        .await?;
    let refused = RpcClient::new(None)?
        .request(&worker.endpoint, Request::Describe)
        .await;
    assert!(refused.unwrap_err().to_string().contains("TlsRequired"));

    // The same worker can also be configured without a TOML file or any PKI.
    let config = worker.config.clone();
    worker
        .restart_with(&RpcClient::new(None)?, |command| {
            *command = isolated_command(WORKER_BINARY);
            command
                .env("DJBOD_NODE_ID", config.node.0.to_string())
                .env("DJBOD_CLUSTER", config.cluster.to_string())
                .env("DJBOD_LISTEN", &address)
                .env(
                    "DJBOD_DEVICES",
                    config
                        .devices
                        .iter()
                        .map(|p| p.to_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .env("DJBOD_BITROTTER_JOURNAL", &config.journal);
        })
        .await?;
    let output = isolated_command(WORKER_BINARY)
        .arg("--tls-ca")
        .arg(&actual.ca)
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr)?;
    assert!(error.contains("--tls-cert") && error.contains("--tls-key"));
    Ok(())
}
