//! `djbod-node` settings from arguments, environment variables, and the
//! configuration file, in that order of precedence (SPEC 20.6).

use std::process::Command;

use djbod_core::cluster::ClusterDocument;
use djbod_node::node::CLUSTER_DOCUMENT_FILE;

fn node_binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_djbod-node"));
    // A clean slate: the test's own environment must not leak in.
    for (key, _) in std::env::vars() {
        if key.starts_with("DJBOD_") {
            command.env_remove(key);
        }
    }
    command
}

fn run(command: &mut Command) -> (bool, String, String) {
    let output = command.output().expect("run djbod-node");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn saved_document(state: &std::path::Path) -> ClusterDocument {
    let text = std::fs::read_to_string(state.join(CLUSTER_DOCUMENT_FILE)).expect("document");
    serde_json::from_str(&text).expect("parses")
}

#[test]
fn a_cluster_can_be_created_from_the_environment_alone() {
    let state = tempfile::tempdir().expect("temp dir");
    let d0 = tempfile::tempdir().expect("temp dir");
    let d1 = tempfile::tempdir().expect("temp dir");
    let node_id = uuid::Uuid::new_v4();
    let (ok, out, err) = run(node_binary()
        .env("DJBOD_NODE_ID", node_id.to_string())
        .env("DJBOD_STATE_DIR", state.path())
        .env(
            "DJBOD_DEVICES",
            format!("{},{}", d0.path().display(), d1.path().display()),
        )
        .env("DJBOD_LISTEN", "127.0.0.1:5999")
        .env("DJBOD_ALLOW_SHARED_FILESYSTEM", "true")
        .args(["init-cluster", "--k", "1", "--m", "1"]));
    assert!(ok, "{err}");
    assert!(out.contains(&format!("node    {node_id}")), "{out}");
    let document = saved_document(state.path());
    assert_eq!(document.nodes[0].id.0, node_id);
    assert_eq!(
        document.nodes[0].addresses,
        vec!["127.0.0.1:5999".to_string()]
    );
    assert_eq!(document.devices.len(), 2);

    // A chosen cluster id is honoured, for provisioning.
    let state2 = tempfile::tempdir().expect("temp dir");
    let d2 = tempfile::tempdir().expect("temp dir");
    let chosen = uuid::Uuid::new_v4();
    let (ok, out, err) = run(node_binary()
        .env("DJBOD_NODE_ID", uuid::Uuid::new_v4().to_string())
        .env("DJBOD_STATE_DIR", state2.path())
        .env("DJBOD_DEVICES", d2.path())
        .env("DJBOD_CLUSTER_ID", chosen.to_string())
        .args(["init-cluster", "--k", "1", "--m", "0"]));
    assert!(ok, "{err}");
    assert!(out.contains(&format!("cluster {chosen} created")), "{out}");
    assert_eq!(saved_document(state2.path()).cluster_id, chosen);

    // Without the required settings the error names what is missing.
    let (ok, _, err) = run(node_binary().args(["init-cluster"]));
    assert!(!ok);
    assert!(err.contains("--node-id or DJBOD_NODE_ID"), "{err}");
}

#[test]
fn an_argument_beats_the_environment_which_beats_the_file() {
    let state = tempfile::tempdir().expect("temp dir");
    let d0 = tempfile::tempdir().expect("temp dir");
    let config_path = state.path().join("node.toml");
    std::fs::write(
        &config_path,
        format!(
            "node_id = \"{}\"\nlisten = \"127.0.0.1:5001\"\nstate_dir = \"{}\"\ndevices = [\"{}\"]\n",
            uuid::Uuid::new_v4(),
            state.path().display(),
            d0.path().display()
        ),
    )
    .expect("write config");

    // File only.
    let (ok, _, err) = run(node_binary()
        .args(["init-cluster", "--k", "1", "--m", "0"])
        .arg("--config")
        .arg(&config_path));
    assert!(ok, "{err}");
    assert_eq!(
        saved_document(state.path()).nodes[0].addresses,
        vec!["127.0.0.1:5001".to_string()]
    );

    // Environment over file: a fresh state and device, since init-cluster
    // needs empty devices; the file's listen is overridden.
    let state2 = tempfile::tempdir().expect("temp dir");
    let d1 = tempfile::tempdir().expect("temp dir");
    let (ok, _, err) = run(node_binary()
        .env("DJBOD_CONFIG", &config_path)
        .env("DJBOD_LISTEN", "127.0.0.1:5002")
        .env("DJBOD_STATE_DIR", state2.path())
        .env("DJBOD_DEVICES", d1.path())
        .args(["init-cluster", "--k", "1", "--m", "0"]));
    assert!(ok, "{err}");
    assert_eq!(
        saved_document(state2.path()).nodes[0].addresses,
        vec!["127.0.0.1:5002".to_string()]
    );

    // Argument over environment.
    let state3 = tempfile::tempdir().expect("temp dir");
    let d2 = tempfile::tempdir().expect("temp dir");
    let (ok, _, err) = run(node_binary()
        .env("DJBOD_CONFIG", &config_path)
        .env("DJBOD_LISTEN", "127.0.0.1:5002")
        .env("DJBOD_STATE_DIR", state3.path())
        .env("DJBOD_DEVICES", d2.path())
        .args([
            "init-cluster",
            "--k",
            "1",
            "--m",
            "0",
            "--listen",
            "127.0.0.1:5003",
        ]));
    assert!(ok, "{err}");
    assert_eq!(
        saved_document(state3.path()).nodes[0].addresses,
        vec!["127.0.0.1:5003".to_string()]
    );

    // The offline scrub takes the same settings and, with --device, looks
    // at that device only.
    let (ok, _, err) = run(node_binary()
        .env("DJBOD_CONFIG", &config_path)
        .env("DJBOD_STATE_DIR", state3.path())
        .arg("scrub")
        .arg("--device")
        .arg(d2.path()));
    assert!(ok, "{err}");
    assert!(
        err.contains(&format!("{}: 0 records", d2.path().display())),
        "{err}"
    );
}
