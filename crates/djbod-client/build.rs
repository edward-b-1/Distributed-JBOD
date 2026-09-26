//! Records the git commit the node is built from, for `Hello.build` and
//! `--version`. `DJBOD_GIT_COMMIT` in the environment overrides it, for
//! builds without a checkout such as containers; "unknown" otherwise.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=DJBOD_GIT_COMMIT");
    let commit = std::env::var("DJBOD_GIT_COMMIT")
        .ok()
        .filter(|commit| !commit.is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=DJBOD_GIT_COMMIT={commit}");
}

fn git_commit() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&manifest_dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    // Rebuild when the checked-out commit changes.
    for path in ["HEAD", "refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    git(&["rev-parse", "--short=9", "HEAD"])
}
