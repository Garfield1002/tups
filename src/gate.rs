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
use crate::sandbox::GATE_SOCK;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{exit, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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
    if op == "push" && !policy.refs.is_empty() {
        if args.is_empty() {
            return Err("this gate requires an explicit ref to push".to_string());
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

// ---------------------------------------------------------------------------
// host side: the gatekeeper
// ---------------------------------------------------------------------------

/// A running gatekeeper: its socket (to forward into the container) and a stop flag.
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

/// Start the gatekeeper on a background thread, serving a socket under the project dir. `tree`
/// is the host path of the working tree the operations run in (bound into the container, so
/// the container and host see the same files).
pub(crate) fn spawn(policy: GatePolicy, project: &str, tree: PathBuf) -> std::io::Result<Gate> {
    serve_on(project_dir(project).join("gate.sock"), policy, tree)
}

/// Bind the gatekeeper to an explicit socket path and start its accept loop.
fn serve_on(sock: PathBuf, policy: GatePolicy, tree: PathBuf) -> std::io::Result<Gate> {
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
                Ok((stream, _)) => handle_conn(stream, &policy, &tree),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
    });
    Ok(Gate { sock, stop })
}

/// Serve one request: read a line, filter it, and (if allowed) run git on the host.
fn handle_conn(mut stream: UnixStream, policy: &GatePolicy, tree: &PathBuf) {
    let mut req = String::new();
    if stream.read_to_string(&mut req).is_err() {
        return;
    }
    let mut it = req.split_whitespace();
    let op = it.next().unwrap_or("").to_string();
    let args: Vec<String> = it.map(str::to_string).collect();

    if let Err(reason) = authorize(policy, &op, &args) {
        let _ = writeln!(stream, "DENIED {reason}");
        return;
    }
    // The remote comes from POLICY, never the request. The op/args were authorized above.
    let out = Command::new("git")
        .arg("-C")
        .arg(tree)
        .arg(&op)
        .arg(&policy.remote)
        .args(&args)
        .output();
    match out {
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            let _ = writeln!(stream, "OK {code}");
            let _ = stream.write_all(&o.stdout);
            let _ = stream.write_all(&o.stderr);
        }
        Err(e) => {
            let _ = writeln!(stream, "OK -1");
            let _ = writeln!(stream, "[tup gatekeeper] failed to run git: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// container side: the gate driver (`tup push` / `tup fetch`)
// ---------------------------------------------------------------------------

/// Drive the git gate from inside the container: send `op args…`, relay the result. The
/// container never sees a credential — it only asks the host to perform the filtered op.
pub(crate) fn drive(op: &str, args: &[String]) -> ! {
    // The forwarded gate socket is at GATE_SOCK for a flake-declared gate; TUP_GATE_SOCK lets
    // a bootstrap / already-running container point at a gate socket in an already-bound dir.
    let sock = std::env::var("TUP_GATE_SOCK").unwrap_or_else(|_| GATE_SOCK.to_string());
    let mut stream = UnixStream::connect(&sock).unwrap_or_else(|e| {
        eprintln!(
            "tup {op}: no git gate at {sock} ({e}).\n  Declare one in the flake (outputs.tup.gates.git) or run `tup gate serve` on the host and set TUP_GATE_SOCK."
        );
        exit(1);
    });
    let req = format!("{op} {}", args.join(" "));
    if stream.write_all(req.trim_end().as_bytes()).is_err() || stream.shutdown(std::net::Shutdown::Write).is_err() {
        eprintln!("tup {op}: could not send request to the gate");
        exit(1);
    }
    let mut resp = String::new();
    if stream.read_to_string(&mut resp).is_err() {
        eprintln!("tup {op}: no response from the gate");
        exit(1);
    }
    let (head, body) = match resp.split_once('\n') {
        Some(x) => x,
        None => (resp.as_str(), ""),
    };
    if let Some(reason) = head.strip_prefix("DENIED ") {
        eprintln!("tup {op}: refused by the gatekeeper — {reason}");
        exit(1);
    }
    if let Some(code) = head.strip_prefix("OK ") {
        print!("{body}");
        let _ = std::io::stdout().flush();
        exit(code.trim().parse::<i32>().unwrap_or(1));
    }
    eprintln!("tup {op}: malformed gate response");
    exit(1);
}

// ---------------------------------------------------------------------------
// `tup gate serve` — a standalone host-side gatekeeper (bootstrap / non-flake use)
// ---------------------------------------------------------------------------

pub(crate) fn cmd_gate(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("serve") => gate_serve(&args[1..]),
        _ => {
            eprintln!(
                "usage: tup gate serve --remote <url> --allow <push,fetch> [--refs <csv>] --sock <path> [--tree <dir>]\n\
                 Runs a gatekeeper on the host (with the host's git creds). Put --sock in a dir\n\
                 bound into the container, then inside set TUP_GATE_SOCK=<that path> and `tup push`."
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

/// Run the gatekeeper in the foreground (blocking) until killed — for `tup gate serve`.
fn serve_foreground(sock: &PathBuf, policy: GatePolicy, tree: &PathBuf) -> ! {
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
    loop {
        match listener.accept() {
            Ok((stream, _)) => handle_conn(stream, &policy, tree),
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
        let gate = serve_on(sock.clone(), policy, tree.clone()).unwrap();
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
}
