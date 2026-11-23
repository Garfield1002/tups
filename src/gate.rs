//! Gates — a filtered escape hatch (`outputs.tup.gates`).
//!
//! The security model: the container holds **no credential and no authority**. A gate is a
//! *capability* — a socket the container can send a request down ("push main"). The **host**
//! is the reference monitor (the "gatekeeper"): it reads the project's declared policy, filters
//! the request (is this op allowed? to the one declared remote? an allowed ref? no option
//! injection?), and only then performs the operation itself, with the host's own credentials,
//! in the project's working tree. The credential never crosses into the container, and the
//! container can do *only* the whitelisted operations — not use the credential for anything
//! else (unlike forwarding an ssh-agent, which can sign anything).
//!
//! First instance: `gates.git` (git `push`/`fetch` to one remote). The container drives it
//! with `tup push`/`tup fetch`; the host runs `git -C <tree> <op> <remote> <refs>`.

use crate::project::project_dir;
use crate::sandbox::{GATE_DIR, GATE_SOCK};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// A named gate the project declares under `outputs.tup.gates`. `name` selects the socket
/// (`<gate dir>/<name>.sock`) and, for the container driver, the `tup gate <name>` verb.
pub(crate) struct GateSpec {
    pub name: String,
    pub kind: GateKind,
}

/// What a gate does on the host. The git gate is the original built-in; a command gate runs a
/// host-authored program (its path comes from the POLICY, never the container) with a single
/// allowlisted argument.
pub(crate) enum GateKind {
    /// The built-in git gate: `git -C <tree> <op> <remote> <args>` under the existing policy.
    Git(GatePolicy),
    /// A user-defined host command: `run <arg>` (no shell), `arg` restricted to `allow`.
    Command { run: String, allow: Vec<String> },
}

/// A project's declared `gates.git` policy. `remote` is the ONLY push/fetch target the
/// container can reach; the container never supplies it.
pub(crate) struct GatePolicy {
    pub remote: String,
    pub allow: Vec<String>,
    /// Allowed refspecs for `push`; empty means "any ref".
    pub refs: Vec<String>,
}

/// Decide whether `op` + `args` are permitted by `policy`. Pure — the security core.
///
/// Rejects: an operation not in `allow`; any arg that looks like an option (`-…`), which would
/// otherwise be git option injection (`--upload-pack=…`); and, for `push` when `refs` is set,
/// any ref outside the allowlist. The remote is never taken from the request.
pub(crate) fn authorize(policy: &GatePolicy, op: &str, args: &[String]) -> Result<(), String> {
    if !policy.allow.iter().any(|a| a == op) {
        return Err(format!(
            "operation '{op}' is not allowed (this gate allows: {})",
            policy.allow.join(", ")
        ));
    }
    for a in args {
        if a.starts_with('-') {
            return Err(format!("argument '{a}' looks like an option; refused"));
        }
    }
    // Destructive push forms are refused regardless of the ref allowlist: a leading '+' is a
    // force-push (history rewrite) and a leading ':' deletes a ref. Neither starts with '-', so
    // the option check above misses them, and with an empty `refs` nothing else would catch them.
    if op == "push" {
        for a in args {
            if let Some(rest) = a.strip_prefix('+') {
                return Err(format!("force-push refspec '+{rest}' is refused"));
            }
            if a.starts_with(':') {
                return Err(format!("ref-deletion refspec '{a}' is refused"));
            }
        }
    }
    // A declared ref allowlist applies to push AND fetch: a fetch refspec writes local refs in
    // the shared working tree, so it is a host action too, not container-only.
    if (op == "push" || op == "fetch") && !policy.refs.is_empty() {
        if args.is_empty() {
            return Err(format!("this gate requires an explicit ref to {op}"));
        }
        for a in args {
            if !policy.refs.iter().any(|r| r == a) {
                return Err(format!(
                    "ref '{a}' is not in this gate's allowed refs ({})",
                    policy.refs.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// Decide whether a command gate may run with `args`. Pure — the command-gate security core.
///
/// Deny-by-default and deliberately minimal: a command gate runs `run <arg>` with exactly ONE
/// argument and no shell. Rejects: a request with other than one argument; an argument not in
/// `allow` (exact match); and an argument that looks like an option (`-…`) unless that exact
/// option string is itself allowlisted. `run` comes from the host-authored policy, never here.
pub(crate) fn authorize_command(run: &str, allow: &[String], args: &[String]) -> Result<(), String> {
    if run.is_empty() {
        return Err("this command gate declares no `run` program".to_string());
    }
    if args.len() != 1 {
        return Err(format!(
            "this gate expects exactly one argument, got {}",
            args.len()
        ));
    }
    let arg = &args[0];
    // An option-looking argument is refused unless it is itself explicitly allowlisted — same
    // care as the git gate, so a `run` program can't be fed `--something` it didn't opt into.
    if arg.starts_with('-') && !allow.iter().any(|a| a == arg) {
        return Err(format!("argument '{arg}' looks like an option; refused"));
    }
    if !allow.iter().any(|a| a == arg) {
        return Err(format!(
            "argument '{arg}' is not allowed (this gate allows: {})",
            allow.join(", ")
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// host side: the gatekeeper
// ---------------------------------------------------------------------------

/// A running gatekeeper: its socket (under the forwarded gate dir) and a stop flag.
pub(crate) struct Gate {
    pub sock: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Gate {
    /// Stop serving and remove the socket (best-effort). Called once the container exits.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// The host directory that holds every gate's socket for a project (`<name>.sock` under it).
/// This whole dir is bound into the container at [`GATE_DIR`].
pub(crate) fn gates_dir(project: &str) -> PathBuf {
    project_dir(project).join("gates")
}

/// Start a gatekeeper for one declared gate on a background thread, serving `<gates
/// dir>/<name>.sock`. `tree` is the host path of the working tree the operations run in (bound
/// into the container, so the container and host see the same files).
pub(crate) fn spawn(spec: GateSpec, project: &str, tree: PathBuf) -> std::io::Result<Gate> {
    let sock = gates_dir(project).join(format!("{}.sock", spec.name));
    serve_on(sock, spec.kind, tree)
}

/// Bind one gate to an explicit socket path and start its accept loop.
fn serve_on(sock: PathBuf, kind: GateKind, tree: PathBuf) -> std::io::Result<Gate> {
    let _ = std::fs::remove_file(&sock);
    if let Some(p) = sock.parent() {
        std::fs::create_dir_all(p)?;
    }
    let listener = UnixListener::bind(&sock)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => handle_conn(stream, &kind, &tree),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });
    Ok(Gate { sock, stop })
}

/// Serve one request: read the line, split into whitespace tokens, and dispatch to the gate
/// kind's handler, which filters and (if allowed) performs the host-side operation.
fn handle_conn(mut stream: UnixStream, kind: &GateKind, tree: &Path) {
    // Bound the read: a container that connects and never closes its write half must not wedge
    // the (single-threaded) accept loop indefinitely.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut req = String::new();
    if stream.read_to_string(&mut req).is_err() {
        return;
    }
    let tokens: Vec<String> = req.split_whitespace().map(str::to_string).collect();
    match kind {
        GateKind::Git(policy) => handle_git(stream, policy, tree, &tokens),
        GateKind::Command { run, allow } => handle_command(stream, run, allow, tree, &tokens),
    }
}

/// The git gate: `tokens[0]` is the op, the rest are refspecs. Filtered by [`authorize`].
fn handle_git(mut stream: UnixStream, policy: &GatePolicy, tree: &Path, tokens: &[String]) {
    let op = tokens.first().cloned().unwrap_or_default();
    let args: Vec<String> = tokens.iter().skip(1).cloned().collect();

    if let Err(reason) = authorize(policy, &op, &args) {
        let _ = writeln!(stream, "DENIED {reason}");
        return;
    }
    // The remote comes from POLICY, never the request. The op/args were authorized above.
    //
    // CRITICAL: `tree` is the working tree bound READ-WRITE into the container, so the container
    // controls `<tree>/.git`. Host-side git must therefore honor none of the repo-local code
    // vectors a hostile hook/repo could plant: hooks (e.g. pre-push), fsmonitor, core.sshCommand,
    // and the ext:: transport (reachable via url.insteadOf). `-c` on the command line beats
    // repo-local config, and GIT_SSH_COMMAND beats a repo-local core.sshCommand. (Residual: a
    // url.insteadOf can still redirect to a different ssh host — it can't run host code, but it
    // can misdirect the push; a clean mirror would close that too.)
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=")
        .arg("-c")
        .arg("protocol.ext.allow=never")
        .arg("-C")
        .arg(tree)
        .arg(&op)
        .arg(&policy.remote)
        .args(&args);
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh");
    }
    relay_output(stream, cmd.output(), "git");
}

/// A user-defined command gate: run `run <arg>` directly (no shell), cwd = the tree. The program
/// path is from the host-authored policy, never the request, and the arg was allowlisted by
/// [`authorize_command`]. The environment is cleaned of inherited `GIT_*` (the same care the git
/// gate takes), so a hostile repo's `GIT_*` can't steer the program.
fn handle_command(mut stream: UnixStream, run: &str, allow: &[String], tree: &Path, args: &[String]) {
    if let Err(reason) = authorize_command(run, allow, args) {
        let _ = writeln!(stream, "DENIED {reason}");
        return;
    }
    let mut cmd = Command::new(run);
    cmd.arg(&args[0]).current_dir(tree); // authorize_command guarantees exactly one arg
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    relay_output(stream, cmd.output(), "command");
}

/// Relay a finished host-side command's result down the socket: `OK <code>` then stdout+stderr,
/// or `OK -1` with a diagnostic if the program could not be launched. Shared by all gate kinds.
fn relay_output(mut stream: UnixStream, out: std::io::Result<Output>, what: &str) {
    match out {
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            let _ = writeln!(stream, "OK {code}");
            let _ = stream.write_all(&o.stdout);
            let _ = stream.write_all(&o.stderr);
        }
        Err(e) => {
            let _ = writeln!(stream, "OK -1");
            let _ = writeln!(stream, "[tup gatekeeper] failed to run {what}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// container side: the gate drivers (`tup gate <name> …`, `tup push` / `tup fetch`)
// ---------------------------------------------------------------------------

/// Resolve the container-side socket path for gate `name`. The gate directory is bound at
/// [`GATE_DIR`]; `TUP_GATE_DIR` overrides it (a bootstrap / already-running container pointing
/// at an already-bound dir). For the git gate, the legacy `TUP_GATE_SOCK` single-socket override
/// and the canonical [`GATE_SOCK`] path still work for back-compat.
fn gate_sock_path(name: &str) -> PathBuf {
    if name == "git" {
        if let Ok(s) = std::env::var("TUP_GATE_SOCK") {
            return PathBuf::from(s);
        }
    }
    match std::env::var("TUP_GATE_DIR") {
        Ok(dir) => PathBuf::from(dir).join(format!("{name}.sock")),
        Err(_) if name == "git" => PathBuf::from(GATE_SOCK),
        Err(_) => PathBuf::from(GATE_DIR).join(format!("{name}.sock")),
    }
}

/// Drive the named gate from inside the container: connect to its socket, send `args…`, relay
/// the result. The container never sees a credential — it only asks the host to perform the
/// filtered op. `tup gate <name> <args…>`.
pub(crate) fn drive(name: &str, args: &[String]) -> ! {
    drive_impl(&format!("gate {name}"), name, args)
}

/// The git-gate aliases `tup push` / `tup fetch`: prepend the op and drive the `git` gate, so
/// existing behavior, docs, and examples keep working byte-for-byte.
pub(crate) fn drive_git(op: &str, args: &[String]) -> ! {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(op.to_string());
    full.extend_from_slice(args);
    drive_impl(op, "git", &full)
}

/// Shared driver body. `label` prefixes user messages (`push`, `gate deploy`, …); `sock_name`
/// selects the socket; `args` are the whitespace-joined request line.
fn drive_impl(label: &str, sock_name: &str, args: &[String]) -> ! {
    let sock = gate_sock_path(sock_name);
    let mut stream = UnixStream::connect(&sock).unwrap_or_else(|e| {
        eprintln!(
            "tup {label}: no gate at {} ({e}).\n  Declare one in the flake (outputs.tup.gates.{sock_name}) or run `tup gate serve` on the host and set TUP_GATE_DIR/TUP_GATE_SOCK.",
            sock.display()
        );
        exit(1);
    });
    let req = args.join(" ");
    if stream.write_all(req.trim_end().as_bytes()).is_err()
        || stream.shutdown(std::net::Shutdown::Write).is_err()
    {
        eprintln!("tup {label}: could not send request to the gate");
        exit(1);
    }
    let mut resp = String::new();
    if stream.read_to_string(&mut resp).is_err() {
        eprintln!("tup {label}: no response from the gate");
        exit(1);
    }
    let (head, body) = match resp.split_once('\n') {
        Some(x) => x,
        None => (resp.as_str(), ""),
    };
    if let Some(reason) = head.strip_prefix("DENIED ") {
        eprintln!("tup {label}: refused by the gatekeeper — {reason}");
        exit(1);
    }
    if let Some(code) = head.strip_prefix("OK ") {
        print!("{body}");
        let _ = std::io::stdout().flush();
        exit(code.trim().parse::<i32>().unwrap_or(1));
    }
    eprintln!("tup {label}: malformed gate response");
    exit(1);
}

// ---------------------------------------------------------------------------
// `tup gate` — host-side standalone gatekeeper (`serve`) + container-side driver (`<name>`)
// ---------------------------------------------------------------------------

pub(crate) fn cmd_gate(args: &[String]) {
    match args.first().map(String::as_str) {
        // Host side: a standalone git gatekeeper (bootstrap / non-flake use).
        Some("serve") => gate_serve(&args[1..]),
        // Container side: drive the named gate (`tup gate <name> [args…]`).
        Some(name) => drive(name, &args[1..]),
        None => {
            eprintln!(
                "usage:\n\
                 tup gate <name> [args…]    (in container) drive the named gate\n\
                 tup gate serve --remote <url> --allow <push,fetch> [--refs <csv>] --sock <path> [--tree <dir>]\n\
                     (on host) run a standalone git gatekeeper with the host's git creds. Put\n\
                     --sock in a dir bound into the container, set TUP_GATE_SOCK=<that path>, `tup push`."
            );
            exit(2);
        }
    }
}

fn gate_serve(args: &[String]) -> ! {
    let mut remote = String::new();
    let mut allow: Vec<String> = Vec::new();
    let mut refs: Vec<String> = Vec::new();
    let mut sock: Option<PathBuf> = None;
    let mut tree = PathBuf::from(".");
    let csv = |s: &str| -> Vec<String> {
        s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
    };
    let mut i = 0;
    while i < args.len() {
        let next = |i: usize| -> String {
            args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("tup gate serve: {} needs a value", args[i]);
                exit(2);
            })
        };
        match args[i].as_str() {
            "--remote" => remote = next(i),
            "--allow" => allow = csv(&next(i)),
            "--refs" => refs = csv(&next(i)),
            "--sock" => sock = Some(PathBuf::from(next(i))),
            "--tree" => tree = PathBuf::from(next(i)),
            other => {
                eprintln!("tup gate serve: unexpected arg '{other}'");
                exit(2);
            }
        }
        i += 2;
    }
    if remote.is_empty() || allow.is_empty() || sock.is_none() {
        eprintln!("tup gate serve: --remote, --allow and --sock are required");
        exit(2);
    }
    let sock = sock.unwrap();
    let tree = std::fs::canonicalize(&tree).unwrap_or(tree);
    let policy = GatePolicy { remote, allow, refs };
    serve_foreground(&sock, policy, &tree)
}

/// Run a standalone git gatekeeper in the foreground (blocking) until killed — `tup gate serve`.
fn serve_foreground(sock: &Path, policy: GatePolicy, tree: &Path) -> ! {
    let _ = std::fs::remove_file(sock);
    if let Some(p) = sock.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let listener = UnixListener::bind(sock).unwrap_or_else(|e| {
        eprintln!("tup gate serve: cannot bind {}: {e}", sock.display());
        exit(1);
    });
    eprintln!(
        "[tup gatekeeper] serving {} — allow={:?} refs={:?} remote={} tree={} (Ctrl-C to stop)",
        sock.display(),
        policy.allow,
        policy.refs,
        policy.remote,
        tree.display()
    );
    let kind = GateKind::Git(policy);
    loop {
        match listener.accept() {
            Ok((stream, _)) => handle_conn(stream, &kind, tree),
            Err(e) => eprintln!("[tup gatekeeper] accept error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], refs: &[&str]) -> GatePolicy {
        GatePolicy {
            remote: "git@github.com:me/repo.git".to_string(),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            refs: refs.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allows_whitelisted_op() {
        assert!(authorize(&policy(&["push", "fetch"], &[]), "push", &args(&["main"])).is_ok());
        assert!(authorize(&policy(&["fetch"], &[]), "fetch", &args(&[])).is_ok());
    }

    #[test]
    fn denies_op_not_in_allowlist() {
        assert!(authorize(&policy(&["fetch"], &[]), "push", &args(&["main"])).is_err());
        // not even clever ones — op must match exactly
        assert!(authorize(&policy(&["push"], &[]), "gc", &args(&[])).is_err());
    }

    #[test]
    fn denies_option_injection() {
        // a crafted "refspec" that is really a git option must be refused
        for bad in [
            vec!["--upload-pack=touch /tmp/x"],
            vec!["main", "--receive-pack=evil"],
            vec!["-o"],
        ] {
            assert!(
                authorize(&policy(&["push"], &[]), "push", &args(&bad)).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn enforces_ref_allowlist_for_push() {
        let p = policy(&["push"], &["main"]);
        assert!(authorize(&p, "push", &args(&["main"])).is_ok());
        assert!(authorize(&p, "push", &args(&["secret-branch"])).is_err());
        // an empty ref list means "any ref"
        assert!(authorize(&policy(&["push"], &[]), "push", &args(&["anything"])).is_ok());
        // with a ref allowlist, a bare push (no ref) is refused — must be explicit
        assert!(authorize(&p, "push", &args(&[])).is_err());
    }

    // --- end-to-end host-side pipe: socket round-trip -> filter -> gatekeeper runs git ---

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    fn send(sock: &std::path::Path, line: &str) -> String {
        let mut s = UnixStream::connect(sock).unwrap();
        s.write_all(line.as_bytes()).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    }

    #[test]
    fn gatekeeper_runs_allowed_op_and_refuses_the_rest() {
        // A real git fetch over a LOCAL bare remote — no network, no credentials.
        let tmp = std::env::temp_dir().join(format!("tup-gate-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bare = tmp.join("remote.git");
        let tree = tmp.join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();
        git(&tree, &["init", "-q", "-b", "main"]);
        git(&tree, &["config", "user.email", "t@t"]);
        git(&tree, &["config", "user.name", "t"]);
        std::fs::write(tree.join("f"), "x").unwrap();
        git(&tree, &["add", "."]);
        git(&tree, &["commit", "-qm", "init"]);
        // Seed the bare remote so an allowed fetch cleanly succeeds (OK 0).
        let bare_str = bare.to_string_lossy().into_owned();
        git(&tree, &["push", "-q", &bare_str, "HEAD:refs/heads/main"]);

        // Gate: fetch allowed, push NOT. Remote is fixed by policy (a local bare repo here).
        let policy = GatePolicy {
            remote: bare.to_string_lossy().into_owned(),
            allow: vec!["fetch".to_string()],
            refs: vec![],
        };
        let sock = tmp.join("gate.sock");
        let gate = serve_on(sock.clone(), GateKind::Git(policy), tree.clone()).unwrap();
        thread::sleep(Duration::from_millis(150)); // let the listener come up

        let allowed = send(&sock, "fetch");
        assert!(allowed.starts_with("OK 0"), "allowed fetch should succeed: {allowed:?}");

        let denied = send(&sock, "push main");
        assert!(denied.starts_with("DENIED "), "push is not in allow: {denied:?}");

        // Option injection is refused at the gate, never reaching git.
        let inj = send(&sock, "fetch --upload-pack=touch");
        assert!(inj.starts_with("DENIED "), "option injection must be refused: {inj:?}");

        gate.stop();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn refuses_force_push_and_delete() {
        let p = policy(&["push"], &[]); // empty refs == "any ref"
        assert!(authorize(&p, "push", &args(&["+refs/heads/main"])).is_err());
        assert!(authorize(&p, "push", &args(&["+main:main"])).is_err());
        assert!(authorize(&p, "push", &args(&[":refs/heads/main"])).is_err());
        assert!(authorize(&p, "push", &args(&["main"])).is_ok());
    }

    #[test]
    fn ref_allowlist_also_covers_fetch() {
        let p = policy(&["fetch"], &["main"]);
        assert!(authorize(&p, "fetch", &args(&["main"])).is_ok());
        assert!(authorize(&p, "fetch", &args(&["refs/heads/secret:refs/heads/x"])).is_err());
    }

    #[test]
    fn gatekeeper_does_not_run_repo_hooks_on_the_host() {
        // A container-planted .git/hooks/pre-push must NOT execute when the host runs the
        // sanctioned push (regression for the gate->host-RCE fix).
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("tup-gate-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bare = tmp.join("remote.git");
        let tree = tmp.join("work");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();
        git(&tree, &["init", "-q", "-b", "main"]);
        git(&tree, &["config", "user.email", "t@t"]);
        git(&tree, &["config", "user.name", "t"]);
        std::fs::write(tree.join("f"), "x").unwrap();
        git(&tree, &["add", "."]);
        git(&tree, &["commit", "-qm", "init"]);

        let sentinel = tmp.join("pwned");
        let hook = tree.join(".git/hooks/pre-push");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", sentinel.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let policy = GatePolicy {
            remote: bare.to_string_lossy().into_owned(),
            allow: vec!["push".to_string()],
            refs: vec![],
        };
        let sock = tmp.join("gate.sock");
        let gate = serve_on(sock.clone(), GateKind::Git(policy), tree.clone()).unwrap();
        thread::sleep(Duration::from_millis(150));
        let resp = send(&sock, "push HEAD:refs/heads/main");
        gate.stop();

        assert!(resp.starts_with("OK 0"), "push should still succeed: {resp:?}");
        assert!(!sentinel.exists(), "repo pre-push hook must NOT execute on the host");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- command gate: pure authorization + end-to-end over a socket ---

    #[test]
    fn authorize_command_allows_only_the_single_allowlisted_arg() {
        let allow = args(&["staging", "prod"]);
        assert!(authorize_command("/x", &allow, &args(&["staging"])).is_ok());
        assert!(authorize_command("/x", &allow, &args(&["prod"])).is_ok());
        // not in the allowlist
        assert!(authorize_command("/x", &allow, &args(&["production"])).is_err());
        // empty `run` is refused outright
        assert!(authorize_command("", &allow, &args(&["staging"])).is_err());
    }

    #[test]
    fn authorize_command_requires_exactly_one_arg() {
        let allow = args(&["staging"]);
        assert!(authorize_command("/x", &allow, &args(&[])).is_err()); // zero
        assert!(authorize_command("/x", &allow, &args(&["staging", "staging"])).is_err()); // two
        assert!(authorize_command("/x", &allow, &args(&["staging"])).is_ok()); // one
    }

    #[test]
    fn authorize_command_refuses_option_injection() {
        // An option-looking arg is refused unless the exact string is itself allowlisted.
        let allow = args(&["staging"]);
        for bad in [vec!["--foo"], vec!["-x"], vec!["--force=1"]] {
            assert!(
                authorize_command("/x", &allow, &args(&bad)).is_err(),
                "{bad:?} should be refused"
            );
        }
        // …but an explicitly-allowed flag-shaped arg is permitted (host opted in).
        let allow_flag = args(&["--dry-run"]);
        assert!(authorize_command("/x", &allow_flag, &args(&["--dry-run"])).is_ok());
    }

    #[test]
    fn command_gate_runs_allowed_arg_and_refuses_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        // A safe host program, authored by the host (never the container): a script that writes
        // its single argument into the tree and exits 0.
        let tmp = std::env::temp_dir().join(format!("tup-cmdgate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let tree = tmp.join("work");
        std::fs::create_dir_all(&tree).unwrap();
        let prog = tmp.join("deploy.sh");
        std::fs::write(&prog, "#!/bin/sh\necho deployed \"$1\"\nexit 0\n").unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();

        let kind = GateKind::Command {
            run: prog.to_string_lossy().into_owned(),
            allow: vec!["staging".to_string()],
        };
        let sock = tmp.join("deploy.sock");
        let gate = serve_on(sock.clone(), kind, tree.clone()).unwrap();
        thread::sleep(Duration::from_millis(150));

        let ok = send(&sock, "staging");
        assert!(ok.starts_with("OK 0"), "allowed arg should run and succeed: {ok:?}");
        assert!(ok.contains("deployed staging"), "program output relayed: {ok:?}");

        let denied = send(&sock, "prod");
        assert!(denied.starts_with("DENIED "), "arg not in allow must be refused: {denied:?}");

        let injected = send(&sock, "--rm-rf");
        assert!(injected.starts_with("DENIED "), "option-looking arg must be refused: {injected:?}");

        let too_many = send(&sock, "staging staging");
        assert!(too_many.starts_with("DENIED "), "more than one arg must be refused: {too_many:?}");

        gate.stop();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
