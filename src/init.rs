//! `tup init` — the in-container PID 1 / supervisor. Starts declared services, reaps the
//! whole process tree, and on a terminating signal runs stop hooks before a bounded
//! SIGTERM→SIGKILL sweep (the clean quiesce a move depends on).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::{exit, Command};
use std::time::{Duration, Instant};

/// How a declared service is respawned when it exits (Docker's `--restart`, per service).
/// `No` is the default and preserves the original behaviour (a service that exits stays exited,
/// and once every declared service has exited the container shuts down).
#[derive(Clone, Copy, PartialEq)]
enum Restart {
    No,
    OnFailure,
    Always,
}

/// Crash-loop guard: at most this many restarts of one service within the window before tup
/// gives up and lets it stay dead (so a service that dies instantly can't spin forever).
const MAX_RESTARTS: u32 = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(10);

struct Service {
    name: String,
    start: Option<String>,
    stop: Option<String>,
    restart: Restart,
}

pub(crate) fn cmd_init(args: &[String]) {
    let project = args.first().map(String::as_str).unwrap_or("?");
    let services = load_services("/run/tup.services");
    let sfd = setup_signalfd();

    let mut running: HashMap<i32, String> = HashMap::new();
    for s in &services {
        if let Some(start) = &s.start {
            match spawn_shell(start) {
                Ok(pid) => {
                    running.insert(pid as i32, s.name.clone());
                    eprintln!("[tup init] started '{}' (pid {pid})", s.name);
                }
                Err(e) => eprintln!("[tup init] failed to start '{}': {e}", s.name),
            }
        }
    }
    let self_pid = unsafe { libc::getpid() };
    let had_services = !running.is_empty();
    let mut restarts: HashMap<String, (u32, Instant)> = HashMap::new();
    eprintln!(
        "[tup init] project={project} pid={self_pid} supervising {} service(s)",
        running.len()
    );

    loop {
        match next_signal(sfd) {
            Some(libc::SIGCHLD) => {
                // Respawn any exited service whose restart policy asks for it (bounded by the
                // crash-loop guard); the rest stay down and count toward the shutdown check.
                for (name, ok) in reap(&mut running) {
                    let policy = services
                        .iter()
                        .find(|s| s.name == name)
                        .map(|s| s.restart)
                        .unwrap_or(Restart::No);
                    if !wants_restart(policy, ok) {
                        continue;
                    }
                    let start = services.iter().find(|s| s.name == name).and_then(|s| s.start.as_ref());
                    if let Some(start) = start {
                        if !allow_restart(&mut restarts, &name) {
                            eprintln!("[tup init] '{name}' is crash-looping (>{MAX_RESTARTS} restarts); giving up");
                            continue;
                        }
                        match spawn_shell(start) {
                            Ok(pid) => {
                                running.insert(pid as i32, name.clone());
                                eprintln!("[tup init] restarting '{name}' (pid {pid})");
                            }
                            Err(e) => eprintln!("[tup init] failed to restart '{name}': {e}"),
                        }
                    }
                }
                // If every declared service has exited, the container's purpose is done — quit
                // rather than lingering as a hollow namespace that blocks `tup up` indefinitely.
                // (A project with zero declared services is a deliberate namespace holder for
                // `tup attach`, so it stays up.)
                if had_services && running.is_empty() {
                    eprintln!("[tup init] all declared services have exited; shutting down");
                    quiesce(&services, &running);
                    exit(0);
                }
            }
            Some(libc::SIGTERM) | Some(libc::SIGINT) => {
                quiesce(&services, &running);
                exit(0);
            }
            _ => {}
        }
    }
}

fn load_services(path: &str) -> Vec<Service> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut svcs: Vec<Service> = vec![];
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kw, r1) = split_first(line);
        let (name, cmd) = split_first(r1);
        if name.is_empty() {
            eprintln!("[tup init] ignoring malformed manifest line: {line}");
            continue;
        }
        let idx = match svcs.iter().position(|s| s.name == name) {
            Some(i) => i,
            None => {
                svcs.push(Service {
                    name: name.to_string(),
                    start: None,
                    stop: None,
                    restart: Restart::No,
                });
                svcs.len() - 1
            }
        };
        match kw {
            "start" => svcs[idx].start = Some(cmd.to_string()),
            "stop" => svcs[idx].stop = Some(cmd.to_string()),
            "restart" => svcs[idx].restart = parse_restart(cmd),
            other => eprintln!("[tup init] unknown manifest keyword '{other}'"),
        }
    }
    svcs
}

/// Parse a `restart <name> <policy>` directive's policy word. Unknown values warn and fall back
/// to `No` (the safe default) rather than failing the whole manifest.
fn parse_restart(s: &str) -> Restart {
    match s.trim() {
        "no" | "never" | "" => Restart::No,
        "on-failure" | "on_failure" => Restart::OnFailure,
        "always" => Restart::Always,
        other => {
            eprintln!("[tup init] unknown restart policy '{other}' (want no|on-failure|always); using 'no'");
            Restart::No
        }
    }
}

/// Should a service that just exited (with `ok` = exited 0) be respawned under `policy`?
fn wants_restart(policy: Restart, ok: bool) -> bool {
    match policy {
        Restart::No => false,
        Restart::Always => true,
        Restart::OnFailure => !ok,
    }
}

/// Crash-loop guard: record a restart for `name` and return whether it is still under the cap
/// within the rolling window. The window resets once it elapses, so an occasional restart long
/// after the last one does not accumulate toward the cap.
fn allow_restart(restarts: &mut HashMap<String, (u32, Instant)>, name: &str) -> bool {
    let now = Instant::now();
    let e = restarts.entry(name.to_string()).or_insert((0, now));
    if now.duration_since(e.1) > RESTART_WINDOW {
        *e = (0, now);
    }
    e.0 += 1;
    e.0 <= MAX_RESTARTS
}

fn split_first(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn setup_signalfd() -> i32 {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGCHLD] {
            libc::sigaddset(&mut mask, s);
        }
        libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC);
        if fd < 0 {
            eprintln!("[tup init] signalfd failed");
            exit(1);
        }
        fd
    }
}

fn next_signal(sfd: i32) -> Option<i32> {
    let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::signalfd_siginfo>();
    let n = unsafe { libc::read(sfd, &mut si as *mut _ as *mut libc::c_void, size) };
    if n as usize == size {
        Some(si.ssi_signo as i32)
    } else {
        None
    }
}

fn spawn_shell(cmd: &str) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    unsafe {
        c.pre_exec(|| {
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            Ok(())
        });
    }
    c.spawn().map(|ch| ch.id())
}

/// Reap every exited child (non-blocking). Returns the declared services that exited, each with
/// whether it exited cleanly (status 0) — the caller decides whether to respawn them.
fn reap(running: &mut HashMap<i32, String>) -> Vec<(String, bool)> {
    let mut exited = Vec::new();
    loop {
        let mut status: i32 = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        if let Some(name) = running.remove(&pid) {
            eprintln!(
                "[tup init] service '{name}' (pid {pid}) exited: {}",
                describe_status(status)
            );
            let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
            exited.push((name, ok));
        }
    }
    exited
}

fn describe_status(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exit {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

fn quiesce(services: &[Service], running: &HashMap<i32, String>) {
    eprintln!("[tup init] quiescing");
    for s in services {
        if let Some(stop) = &s.stop {
            eprintln!("[tup init] stop hook: '{}'", s.name);
            if let Ok(pid) = spawn_shell(stop) {
                wait_bounded(pid as i32, 5000);
            }
        }
    }

    let svc_pids: HashSet<i32> = running.keys().copied().collect();
    let others = other_pids(&svc_pids);
    if !others.is_empty() {
        let list = others
            .iter()
            .map(|(p, c)| format!("{c}({p})"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[tup init] WARNING: {} process(es) beyond declared service entrypoints were running: {list}",
            others.len()
        );
    }

    unsafe { libc::kill(-1, libc::SIGTERM) };
    let mut discard = HashMap::new();
    for _ in 0..30 {
        let _ = reap(&mut discard);
        if only_init_left() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(-1, libc::SIGKILL) };
    // SIGKILL is async: reap in a short loop so "stopped cleanly" doesn't precede actual reaping.
    for _ in 0..10 {
        let _ = reap(&mut discard);
        if only_init_left() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("[tup init] stopped cleanly");
}

fn wait_bounded(pid: i32, ms: u64) {
    let step = 50u64;
    let mut waited = 0u64;
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid || r < 0 || waited >= ms {
            return;
        }
        std::thread::sleep(Duration::from_millis(step));
        waited += step;
    }
}

fn other_pids(exclude: &HashSet<i32>) -> Vec<(i32, String)> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() {
                if pid == 1 || exclude.contains(&pid) {
                    continue;
                }
                let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                v.push((pid, if comm.is_empty() { "?".into() } else { comm }));
            }
        }
    }
    v
}

fn only_init_left() -> bool {
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() {
                if pid != 1 {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first_separates_keyword_from_rest() {
        assert_eq!(split_first("start foo bar baz"), ("start", "foo bar baz"));
        assert_eq!(split_first("solo"), ("solo", ""));
        assert_eq!(split_first("a  b"), ("a", "b")); // collapses the gap after the first token
        assert_eq!(split_first(""), ("", ""));
    }

    #[test]
    fn load_services_parses_manifest() {
        // A unique temp manifest; load_services takes a path, so no container needed.
        let p = std::env::temp_dir().join(format!(
            "tup-test-svcs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &p,
            "# a comment\n\nstart web run-server\nstop  web graceful-stop\nstart db dbd\n",
        )
        .unwrap();
        let svcs = load_services(p.to_str().unwrap());
        fs::remove_file(&p).ok();

        assert_eq!(svcs.len(), 2); // web and db; comment + blank skipped
        let web = svcs.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.start.as_deref(), Some("run-server"));
        assert_eq!(web.stop.as_deref(), Some("graceful-stop"));
        let db = svcs.iter().find(|s| s.name == "db").unwrap();
        assert_eq!(db.start.as_deref(), Some("dbd"));
        assert_eq!(db.stop, None); // no stop hook declared
    }

    #[test]
    fn load_services_missing_file_is_empty() {
        assert!(load_services("/nonexistent/tup/services/path").is_empty());
    }

    #[test]
    fn restart_policy_parses_and_defaults() {
        assert!(parse_restart("always") == Restart::Always);
        assert!(parse_restart("on-failure") == Restart::OnFailure);
        assert!(parse_restart("no") == Restart::No);
        assert!(parse_restart("bogus") == Restart::No); // unknown → safe default
    }

    #[test]
    fn load_services_reads_restart_directive() {
        let p = std::env::temp_dir().join(format!(
            "tup-test-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&p, "start web run\nrestart web always\nstart db dbd\n").unwrap();
        let svcs = load_services(p.to_str().unwrap());
        fs::remove_file(&p).ok();
        let web = svcs.iter().find(|s| s.name == "web").unwrap();
        assert!(web.restart == Restart::Always);
        let db = svcs.iter().find(|s| s.name == "db").unwrap();
        assert!(db.restart == Restart::No); // unspecified → default
    }

    #[test]
    fn wants_restart_honors_policy() {
        assert!(!wants_restart(Restart::No, false));
        assert!(wants_restart(Restart::Always, true));
        assert!(wants_restart(Restart::OnFailure, false)); // failed → restart
        assert!(!wants_restart(Restart::OnFailure, true)); // clean exit → stay down
    }

    #[test]
    fn allow_restart_caps_a_crash_loop() {
        let mut r = HashMap::new();
        // MAX_RESTARTS allowed, then the guard trips.
        for _ in 0..MAX_RESTARTS {
            assert!(allow_restart(&mut r, "svc"));
        }
        assert!(!allow_restart(&mut r, "svc"), "should trip after the cap");
    }
}
