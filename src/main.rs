//! tup — ultra-micro containers for coding agents.
//!
//! Increment 1: `tup run`  — one-shot command in an isolated namespace.
//! Increment 2: `tup up`/`tup init` — PID-1 supervisor that owns the process tree.
//! Increment 3: per-project network namespace via pasta — own loopback, move-invariant
//!   internal addressing, open NAT egress.
//!
//! Networking model: pasta creates the net+user namespace and runs bwrap inside it
//! (bwrap does fs/pid/ipc/uts but not --unshare-net, inheriting pasta's isolated netns).
//! Egress is NAT'd to the host's default route; DNS is relayed via pasta --dns-forward
//! because the host's resolver is a loopback stub (systemd-resolved) unreachable from a
//! fresh netns. Because pasta does not pass fds through, the sandbox PID 1 is located by
//! scanning /proc for its cmdline rather than bwrap --info-fd.
//!
//! Slice shortcuts still open (see DESIGN.md "Increments"):
//!   - root is the shared host /usr, not a Nix closure; dedicated root = increment 4/7
//!   - services manifest is a line format standing in for the flake `outputs.tup`

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// In-namespace address the guest sends DNS to; pasta intercepts and forwards it to the
/// host resolver. Must be non-loopback (pasta rejects loopback forward addresses).
const PASTA_DNS: &str = "169.254.0.53";

#[derive(Clone, Copy, PartialEq)]
enum Net {
    Pasta,
    Host,
    None,
}

impl Net {
    fn label(self) -> &'static str {
        match self {
            Net::Pasta => "pasta",
            Net::Host => "host",
            Net::None => "none",
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("help") {
        "run" => cmd_run(&args[2..]),
        "up" => cmd_up(&args[2..]),
        "init" => cmd_init(&args[2..]),
        "move" => cmd_move(&args[2..]),
        "preflight" => cmd_preflight(&args[2..]),
        "ps" | "attach" => stub(&args[1]),
        "version" | "--version" | "-V" => println!("tup {VERSION}"),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("tup: unknown command '{other}'\n");
            print_help();
            exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// filesystem / network assembly
// ---------------------------------------------------------------------------

fn project_dir(project: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".tup").join(project)
}

fn ensure_project_root(project: &str) -> PathBuf {
    if project.is_empty() || project.contains('/') || project == "." || project == ".." {
        eprintln!("tup: invalid project name '{project}'");
        exit(2);
    }
    let root = project_dir(project).join("root");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("tup: cannot create {}: {e}", root.display());
        exit(1);
    }
    root
}

/// Write the per-project resolv.conf pointing at the pasta DNS relay.
fn ensure_resolv(project: &str) -> PathBuf {
    let dir = project_dir(project);
    let _ = fs::create_dir_all(&dir);
    let p = dir.join("resolv.conf");
    let _ = fs::write(&p, format!("nameserver {PASTA_DNS}\n"));
    p
}

fn push3(a: &mut Vec<OsString>, flag: &str, x: impl Into<OsString>, y: impl Into<OsString>) {
    a.push(flag.into());
    a.push(x.into());
    a.push(y.into());
}

/// Build bwrap's argument vector (everything up to, but not including, the trailing
/// `--` and the command).
fn bwrap_args(root: &Path, project: &str, net: Net, chdir: &str) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    for s in [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
    ] {
        a.push(s.into());
    }
    if net == Net::None {
        a.push("--unshare-net".into()); // isolated, no egress
    }
    // Shared read-only system layer, on a fresh root (so we can create our own mountpoints).
    for p in ["/usr", "/etc"] {
        if Path::new(p).exists() {
            push3(&mut a, "--ro-bind", p, p);
        }
    }
    for name in ["bin", "sbin", "lib", "lib64", "libx32", "lib32"] {
        let p = format!("/{name}");
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                if let Ok(target) = fs::read_link(&p) {
                    push3(&mut a, "--symlink", target, p);
                }
            }
            Ok(_) => push3(&mut a, "--ro-bind", p.clone(), p),
            Err(_) => {}
        }
    }
    if Path::new("/nix").exists() {
        push3(&mut a, "--ro-bind", "/nix", "/nix");
    }
    // Dedicated read-write project root.
    push3(&mut a, "--bind", root, "/work");
    // Fresh virtual filesystems (--proc and --tmpfs each take a single dest; --dev too).
    for (flag, dest) in [("--proc", "/proc"), ("--dev", "/dev"), ("--tmpfs", "/tmp")] {
        a.push(flag.into());
        a.push(dest.into());
    }
    a.push("--chdir".into());
    a.push(chdir.into());
    push3(&mut a, "--setenv", "TUP_PROJECT", project);
    push3(&mut a, "--setenv", "TUP_NET", net.label());
    // DNS relay for the isolated netns.
    if net == Net::Pasta {
        let resolv = ensure_resolv(project);
        let target =
            fs::canonicalize("/etc/resolv.conf").unwrap_or_else(|_| PathBuf::from("/etc/resolv.conf"));
        push3(&mut a, "--ro-bind", resolv, target);
    }
    a
}

/// Assemble the full command: pasta(outer) + bwrap(inner) for Net::Pasta, else bwrap.
fn assemble(
    root: &Path,
    project: &str,
    net: Net,
    chdir: &str,
    extra_bwrap: Vec<OsString>,
    inner: Vec<OsString>,
) -> Command {
    let mut bw = bwrap_args(root, project, net, chdir);
    bw.extend(extra_bwrap);
    bw.push("--".into());
    bw.extend(inner);

    match net {
        Net::Pasta => {
            let mut c = Command::new("pasta");
            c.arg("--config-net")
                .arg("--dns-forward")
                .arg(PASTA_DNS)
                .arg("--quiet");
            c.arg("--").arg("bwrap");
            c.args(&bw);
            c
        }
        Net::Host | Net::None => {
            let mut c = Command::new("bwrap");
            c.args(&bw);
            c
        }
    }
}

/// Pull an optional `--net <mode>` out of the argument list.
fn parse_net(args: &[String]) -> (Net, Vec<String>) {
    let mut net = Net::Pasta;
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--net" {
            let v = args.get(i + 1).map(String::as_str);
            net = match v {
                Some("pasta") => Net::Pasta,
                Some("host") => Net::Host,
                Some("none") => Net::None,
                other => {
                    eprintln!("tup: unknown --net '{}'", other.unwrap_or(""));
                    exit(2);
                }
            };
            i += 2;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    (net, out)
}

// ---------------------------------------------------------------------------
// tup run — one-shot command
// ---------------------------------------------------------------------------

fn cmd_run(args: &[String]) {
    let (opts, cmd) = match args.iter().position(|a| a == "--") {
        Some(p) => (&args[..p], args[p + 1..].to_vec()),
        None => (args, Vec::new()),
    };

    // Parse options: --net, --bind (rw), --ro-bind, --chdir, then the positional project.
    let mut net = Net::Pasta;
    let mut chdir: Option<String> = None;
    let mut extra: Vec<OsString> = Vec::new();
    let mut project: Option<String> = None;
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--net" => {
                net = match opts.get(i + 1).map(String::as_str) {
                    Some("pasta") => Net::Pasta,
                    Some("host") => Net::Host,
                    Some("none") => Net::None,
                    other => {
                        eprintln!("tup: unknown --net '{}'", other.unwrap_or(""));
                        exit(2);
                    }
                };
                i += 2;
            }
            "--bind" | "--ro-bind" => {
                let flag = if opts[i] == "--bind" { "--bind" } else { "--ro-bind" };
                match opts.get(i + 1) {
                    Some(p) => push3(&mut extra, flag, p.clone(), p.clone()),
                    None => {
                        eprintln!("tup: {} needs a path", opts[i]);
                        exit(2);
                    }
                }
                i += 2;
            }
            "--chdir" => {
                chdir = opts.get(i + 1).cloned();
                if chdir.is_none() {
                    eprintln!("tup: --chdir needs a path");
                    exit(2);
                }
                i += 2;
            }
            _ => {
                if project.is_none() {
                    project = Some(opts[i].clone());
                } else {
                    eprintln!("tup: unexpected argument '{}'", opts[i]);
                    exit(2);
                }
                i += 1;
            }
        }
    }
    let project = match project {
        Some(p) => p,
        None => {
            eprintln!("usage: tup run [--net M] [--bind P] [--ro-bind P] [--chdir P] <project> [-- <cmd>]");
            exit(2);
        }
    };
    let cmd = if cmd.is_empty() {
        vec!["/bin/bash".to_string()]
    } else {
        cmd
    };
    let root = ensure_project_root(&project);
    let chdir = chdir.unwrap_or_else(|| "/work".to_string());
    let inner: Vec<OsString> = cmd.iter().map(OsString::from).collect();

    eprintln!("[tup] run project={project} net={} chdir={chdir}", net.label());
    let mut c = assemble(&root, &project, net, &chdir, extra, inner);
    match c.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("tup: failed to launch container: {e}");
            exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// tup up — launch a supervised container (host side)
// ---------------------------------------------------------------------------

static CHILD_PID: AtomicI32 = AtomicI32::new(-1); // direct child (pasta or bwrap), fallback
static INIT_PID: AtomicI32 = AtomicI32::new(-1); // sandbox PID 1, host-visible pid

/// Forward a terminating signal to the sandbox PID 1 directly, so `tup init` runs its
/// quiesce. Never signal the monitor — that triggers pdeathsig SIGKILL and skips quiesce.
extern "C" fn forward_term(_sig: libc::c_int) {
    let p = INIT_PID.load(Ordering::SeqCst);
    let target = if p > 0 {
        p
    } else {
        CHILD_PID.load(Ordering::SeqCst)
    };
    if target > 0 {
        unsafe { libc::kill(target, libc::SIGTERM) };
    }
}

fn install_forwarders() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        let h: extern "C" fn(libc::c_int) = forward_term;
        sa.sa_sigaction = h as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// Find the host-visible pid of a project's `tup init` by matching its cmdline.
fn find_init_pid(project: &str) -> Option<i32> {
    let rd = fs::read_dir("/proc").ok()?;
    for e in rd.flatten() {
        let pid = match e.file_name().to_string_lossy().parse::<i32>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let data = match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let parts: Vec<&[u8]> = data.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3
            && parts[0] == b"/run/tup"
            && parts[1] == b"init"
            && parts[2] == project.as_bytes()
        {
            return Some(pid);
        }
    }
    None
}

fn poll_init_pid(project: &str, tries: u32) -> Option<i32> {
    for _ in 0..tries {
        if let Some(p) = find_init_pid(project) {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn cmd_up(args: &[String]) {
    let (net, rest) = parse_net(args);
    if rest.is_empty() {
        eprintln!("usage: tup up [--net pasta|host|none] <project>");
        exit(2);
    }
    let project = &rest[0];
    let root = ensure_project_root(project);
    let exe = env::current_exe().unwrap_or_else(|e| {
        eprintln!("tup: cannot find own path: {e}");
        exit(1);
    });
    let services_file = project_dir(project).join("services");

    let mut extra: Vec<OsString> = Vec::new();
    push3(&mut extra, "--ro-bind", exe, "/run/tup");
    if services_file.exists() {
        push3(&mut extra, "--ro-bind", services_file, "/run/tup.services");
    }
    extra.push("--as-pid-1".into());

    let inner: Vec<OsString> = ["/run/tup", "init", project]
        .iter()
        .map(OsString::from)
        .collect();

    let mut c = assemble(&root, project, net, "/work", extra, inner);
    install_forwarders();
    eprintln!("[tup] up project={project} net={} (PID-1 supervisor)", net.label());
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("tup: failed to launch container: {e}");
            exit(1);
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    match poll_init_pid(project, 100) {
        Some(p) => INIT_PID.store(p, Ordering::SeqCst),
        None => eprintln!("[tup] warning: could not locate init pid; shutdown may be abrupt"),
    }
    let status = child.wait().unwrap_or_else(|e| {
        eprintln!("tup: wait failed: {e}");
        exit(1);
    });
    exit(status.code().unwrap_or(0));
}

// ---------------------------------------------------------------------------
// tup init — PID 1 supervisor (inside the container)
// ---------------------------------------------------------------------------

struct Service {
    name: String,
    start: Option<String>,
    stop: Option<String>,
}

fn cmd_init(args: &[String]) {
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
    eprintln!(
        "[tup init] project={project} pid={self_pid} supervising {} service(s)",
        running.len()
    );

    loop {
        match next_signal(sfd) {
            Some(libc::SIGCHLD) => reap(&mut running),
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
                });
                svcs.len() - 1
            }
        };
        match kw {
            "start" => svcs[idx].start = Some(cmd.to_string()),
            "stop" => svcs[idx].stop = Some(cmd.to_string()),
            other => eprintln!("[tup init] unknown manifest keyword '{other}'"),
        }
    }
    svcs
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

fn reap(running: &mut HashMap<i32, String>) {
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
        }
    }
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
        reap(&mut discard);
        if only_init_left() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(-1, libc::SIGKILL) };
    reap(&mut discard);
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

// ---------------------------------------------------------------------------
// tup preflight / tup move — the between-turn migration
// ---------------------------------------------------------------------------

/// The move payload for the prototype is the dedicated rw root (the working tree +
/// any persist files a service writes there) plus the services manifest. Derived state
/// that would normally be symlinks into shared stores isn't modelled yet, so for now the
/// whole root ships.
fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(ft) if ft.is_dir() => total += dir_size(&e.path()),
                Ok(ft) if ft.is_file() => {
                    if let Ok(m) = e.metadata() {
                        total += m.len();
                    }
                }
                _ => {}
            }
        }
    }
    total
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} {}", U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Report what a move would ship or lose. Honest about the current limits.
fn preflight_report(project: &str) {
    let root = project_dir(project).join("root");
    if !root.exists() {
        eprintln!("[preflight] project '{project}' has no root at {}", root.display());
        exit(1);
    }
    let bytes = dir_size(&root);
    let running = find_init_pid(project).is_some();
    eprintln!("[preflight] project={project}");
    eprintln!(
        "  running   : {}",
        if running {
            "yes — will be quiesced at the move boundary"
        } else {
            "no"
        }
    );
    eprintln!("  payload   : {} ({bytes} bytes)  {}", human(bytes), root.display());
    eprintln!("  host-pins : none (link feature not yet implemented)");
    eprintln!("  note      : undeclared processes are reported by the source's quiesce");
}

fn cmd_preflight(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: tup preflight <project>");
        exit(2);
    }
    preflight_report(&args[0]);
}

fn wait_for_gone(pid: i32, ms: u64) -> bool {
    let step = 50u64;
    let mut waited = 0u64;
    loop {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true; // ESRCH — process gone
        }
        if waited >= ms {
            return false;
        }
        std::thread::sleep(Duration::from_millis(step));
        waited += step;
    }
}

/// Spawn a process fully detached (own session, io to a log) so it survives us exiting.
fn spawn_detached(program: &Path, args: &[String], log: &Path) -> bool {
    let f = match File::create(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tup: cannot create log {}: {e}", log.display());
            return false;
        }
    };
    let err = f.try_clone();
    let mut c = Command::new(program);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::from(f));
    if let Ok(e) = err {
        c.stderr(Stdio::from(e));
    }
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn().is_ok()
}

fn run_rsync(args: &[&str]) {
    match Command::new("rsync").args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("tup: rsync failed ({s})");
            exit(1);
        }
        Err(e) => {
            eprintln!("tup: rsync error: {e} (is rsync installed?)");
            exit(1);
        }
    }
}

fn cmd_move(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: tup move <project> <local:<newproject> | [user@]host>");
        exit(2);
    }
    let project = &args[0];
    let dest = &args[1];

    preflight_report(project);

    // Quiesce the source at the (simulated) turn boundary: signal PID 1 directly and
    // wait for the container to tear down, so stop hooks flush state into the root.
    if let Some(pid) = find_init_pid(project) {
        eprintln!("[tup] quiescing source (init pid {pid})");
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if !wait_for_gone(pid, 10_000) {
            eprintln!("[tup] warning: source still running after 10s; shipping anyway");
        }
    } else {
        eprintln!("[tup] source not running; shipping current on-disk state");
    }

    match dest.strip_prefix("local:") {
        Some(newproj) => local_move(project, newproj),
        None => remote_move(project, dest),
    }
}

fn local_move(project: &str, newproj: &str) {
    if newproj.is_empty() || newproj.contains('/') {
        eprintln!("tup: invalid target project '{newproj}'");
        exit(2);
    }
    let src_root = project_dir(project).join("root");
    let dst_dir = project_dir(newproj);
    let dst_root = dst_dir.join("root");
    let _ = fs::create_dir_all(&dst_root);

    let s = format!("{}/", src_root.display());
    let d = format!("{}/", dst_root.display());
    run_rsync(&["-a", "--delete", &s, &d]);

    let src_svc = project_dir(project).join("services");
    if src_svc.exists() {
        let _ = fs::copy(&src_svc, dst_dir.join("services"));
    }
    eprintln!("[tup] shipped root: {project} -> {newproj}");

    // Bring the project up on the "target" (same host, new root) and confirm it resumed.
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let log = dst_dir.join("up.log");
    spawn_detached(&exe, &["up".to_string(), newproj.to_string()], &log);
    match poll_init_pid(newproj, 150) {
        Some(p) => eprintln!("[tup] resumed as '{newproj}' (init pid {p}); log {}", log.display()),
        None => eprintln!("[tup] warning: '{newproj}' did not come up; see {}", log.display()),
    }
}

fn remote_move(project: &str, dest: &str) {
    // Ship to ~/.tup/<project> on the remote (rsync remote paths are relative to $HOME),
    // then start it there detached. Assumes `tup` is on PATH on the remote.
    let src_root = project_dir(project).join("root");
    let s = format!("{}/", src_root.display());
    let remote_root = format!("{dest}:.tup/{project}/root/");

    ssh(dest, &format!("mkdir -p .tup/{project}/root"));
    run_rsync(&["-a", "--delete", "-e", "ssh", &s, &remote_root]);

    let src_svc = project_dir(project).join("services");
    if src_svc.exists() {
        if let Some(svc) = src_svc.to_str() {
            run_rsync(&["-a", "-e", "ssh", svc, &format!("{dest}:.tup/{project}/services")]);
        }
    }
    eprintln!("[tup] shipped root: {project} -> {dest}");

    let up = format!("setsid nohup tup up {project} > .tup/{project}/up.log 2>&1 < /dev/null &");
    ssh(dest, &up);
    eprintln!("[tup] requested start on {dest}; confirm with: ssh {dest} tup preflight {project}");
}

fn ssh(dest: &str, cmd: &str) {
    match Command::new("ssh").arg(dest).arg(cmd).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("[tup] ssh {dest} failed ({s}): {cmd}"),
        Err(e) => eprintln!("[tup] ssh error: {e}"),
    }
}

// ---------------------------------------------------------------------------
// stubs + help
// ---------------------------------------------------------------------------

fn stub(cmd: &str) {
    let planned = match cmd {
        "ps" => "list running tup projects and their declared services",
        "move" => "migrate a session to another host (between-turn, over SSH + rsync)",
        "attach" => "attach a roaming client to a running project",
        "preflight" => "report what a move would lose/ship (undeclared procs, host-pins, bytes)",
        _ => "planned",
    };
    eprintln!("tup {cmd}: not implemented yet — planned: {planned}");
    exit(69);
}

fn print_help() {
    println!(
        "tup {VERSION} — ultra-micro containers for coding agents

USAGE:
    tup <command> [args]

COMMANDS:
    run [--net M] <project> [-- <cmd>]   run a command in a project's namespace   [implemented]
    up  [--net M] <project>              launch a PID-1 supervised container       [implemented]
    ps                                   list running projects and services        [planned]
    move <project> <dest>                migrate a session (local:<new> or host)   [implemented]
    attach <project>                     attach a roaming client                   [planned]
    preflight <project>                  report the cost/loss of a move            [implemented]
    help | version

    --net pasta  (default) dedicated netns, own loopback, NAT egress via pasta
    --net host   share the host network namespace
    --net none   isolated netns, loopback only, no egress

Services are declared in ~/.tup/<project>/services, one directive per line:
    start <name> <shell command>
    stop  <name> <shell command>   (optional clean-quiesce hook)

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model."
    );
}
