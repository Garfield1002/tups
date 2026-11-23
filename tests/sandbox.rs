//! Namespace-level integration smoke tests. These actually build a tup container, so they catch
//! the class of launch/mount/exec regressions the pure-logic unit tests cannot (the overlapping
//! ro-bind crash, the pasta launcher trampoline, the resolv-onto-ro-/etc EROFS). Each test SKIPS
//! (passes with a note) when unprivileged user namespaces — or pasta — aren't available, so the
//! suite stays green on machines that can't run them.

use std::path::PathBuf;
use std::process::Command;

fn tup() -> &'static str {
    env!("CARGO_BIN_EXE_tup")
}

fn uniq(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn rm(proj: &str) {
    let _ = Command::new(tup()).args(["rm", "--force", proj]).output();
}

/// Run `tup run <proj> --net <net> -- <cmd...>`. Returns `None` (skip) when the environment lacks
/// unprivileged user namespaces; otherwise `(success, stderr)`.
fn run(proj: &str, net: &str, cmd: &[&str]) -> Option<(bool, String)> {
    let mut a: Vec<&str> = vec!["run", proj, "--net", net, "--"];
    a.extend_from_slice(cmd);
    let out = Command::new(tup()).args(&a).output().expect("spawn tup");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() && (err.contains("user namespace") || err.contains("unshare")) {
        return None;
    }
    Some((out.status.success(), err))
}

fn pasta_present() -> bool {
    PathBuf::from("/usr/bin/pasta").exists()
        || Command::new("pasta")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

#[test]
fn run_none_launches_and_exits_clean() {
    let p = uniq("it-none");
    let Some((ok, err)) = run(&p, "none", &["/bin/true"]) else {
        eprintln!("skip: no unprivileged user namespaces");
        return;
    };
    rm(&p);
    assert!(ok, "tup run --net none -- true failed:\n{err}");
}

#[test]
fn writes_outside_work_are_ephemeral() {
    // A write to the container's '/' (a fresh tmpfs root) must never appear on the host.
    let marker = "/tup-iso-marker-should-not-escape";
    let _ = std::fs::remove_file(marker);
    let p = uniq("it-iso");
    let Some((ok, err)) = run(
        &p,
        "none",
        &["/bin/sh", "-c", "echo x > /tup-iso-marker-should-not-escape"],
    ) else {
        eprintln!("skip: no unprivileged user namespaces");
        return;
    };
    rm(&p);
    assert!(ok, "container write failed:\n{err}");
    assert!(
        !PathBuf::from(marker).exists(),
        "a write to the container's '/' leaked onto the host at {marker}"
    );
}

#[test]
fn run_pasta_launches_when_pasta_is_present() {
    // Regression for the pasta launcher trampoline + resolv-onto-ro-/etc mount. Skips if pasta
    // (or userns) is unavailable.
    if !pasta_present() {
        eprintln!("skip: pasta not installed");
        return;
    }
    let p = uniq("it-pasta");
    let Some((ok, err)) = run(&p, "pasta", &["/bin/true"]) else {
        eprintln!("skip: no unprivileged user namespaces");
        return;
    };
    rm(&p);
    assert!(ok, "tup run --net pasta -- true failed:\n{err}");
}
