//! bags — ultra-micro containers for coding agents.
//!
//! Increment 1: `bags run`  — one-shot command in an isolated namespace.
//! Increment 2: `bags up`/`bags init` — a PID-1 supervisor that owns the process tree,
//!   starts declared services, reaps orphans, and quiesces cleanly on signal.
//!
//! Slice shortcuts still open (see DESIGN.md "Increments"):
//!   - host networking is shared; dedicated netns via pasta = increment 3
//!   - root is the shared host /usr, not a Nix closure; dedicated root = increment 4/7
//!   - services manifest is a line format standing in for the flake `outputs.bags` = increment 7

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("help") {
        "run" => cmd_run(&args[2..]),
        "up" => cmd_up(&args[2..]),
        "init" => cmd_init(&args[2..]),
        "ps" | "move" | "attach" | "preflight" => stub(&args[1]),
        "version" | "--version" | "-V" => println!("bags {VERSION}"),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("bags: unknown command '{other}'\n");
            print_help();
            exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// shared: filesystem assembly
// ---------------------------------------------------------------------------

/// Bind the shared, read-only system layer onto a fresh container root.
fn add_system_layer(c: &mut Command) {
    for p in ["/usr", "/etc"] {
        if Path::new(p).exists() {
            c.arg("--ro-bind").arg(p).arg(p);
        }
    }
    for name in ["bin", "sbin", "lib", "lib64", "libx32", "lib32"] {
        let p = format!("/{name}");
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                if let Ok(target) = fs::read_link(&p) {
                    c.arg("--symlink").arg(target).arg(&p);
                }
            }
            Ok(_) => {
                c.arg("--ro-bind").arg(&p).arg(&p);
            }
            Err(_) => {}
        }
    }
    if Path::new("/nix").exists() {
        c.arg("--ro-bind").arg("/nix").arg("/nix");
    }
}

/// Common bwrap namespace + virtual-fs flags for a project container.
fn add_container_base(c: &mut Command, root: &Path, project: &str) {
    c.args([
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
    ]);
    add_system_layer(c);
    c.arg("--bind").arg(root).arg("/work");
    c.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    c.args(["--chdir", "/work"]);
    c.args(["--setenv", "BAGS_PROJECT", project]);
}

fn project_dir(project: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".bags").join(project)
}

fn ensure_project_root(project: &str) -> PathBuf {
    if project.is_empty() || project.contains('/') || project == "." || project == ".." {
        eprintln!("bags: invalid project name '{project}'");
        exit(2);
    }
    let root = project_dir(project).join("root");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("bags: cannot create {}: {e}", root.display());
        exit(1);
    }
    root
}

// ---------------------------------------------------------------------------
// bags run — one-shot command
// ---------------------------------------------------------------------------

fn cmd_run(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: bags run <project> [-- <cmd...>]");
        exit(2);
    }
    let project = &args[0];
    let rest = &args[1..];
    let cmd: Vec<String> = match rest.iter().position(|a| a == "--") {
        Some(pos) => rest[pos + 1..].to_vec(),
        None => rest.to_vec(),
    };
    let cmd = if cmd.is_empty() {
        vec!["/bin/bash".to_string()]
    } else {
        cmd
    };

    let root = ensure_project_root(project);
    let mut c = Command::new("bwrap");
    add_container_base(&mut c, &root, project);
    c.arg("--");
    c.args(&cmd);

    eprintln!(
        "[bags] run project={project} root={} (shared rootfs ro, host net, no supervisor)",
        root.display()
    );
    match c.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("bags: failed to exec bwrap: {e} (is bubblewrap installed?)");
            exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// bags up — launch a supervised container (host side)
// ---------------------------------------------------------------------------

static CHILD_PID: AtomicI32 = AtomicI32::new(-1); // bwrap monitor (fallback)
static INIT_PID: AtomicI32 = AtomicI32::new(-1); // sandbox PID 1, host-visible pid

/// Forward a terminating signal to the sandbox PID 1 *directly* (never to the bwrap
/// monitor — killing the monitor triggers PR_SET_PDEATHSIG SIGKILL and skips quiesce).
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

/// Read bwrap's `--info-fd` JSON and extract the sandbox `child-pid` (host pid of PID 1).
fn read_child_pid(fd: i32) -> Option<i32> {
    let mut buf = [0u8; 512];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n <= 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&buf[..n as usize]);
    let key = "\"child-pid\":";
    let i = s.find(key)?;
    s[i + key.len()..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
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

fn cmd_up(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: bags up <project>");
        exit(2);
    }
    let project = &args[0];
    let root = ensure_project_root(project);
    let exe = env::current_exe().unwrap_or_else(|e| {
        eprintln!("bags: cannot find own path: {e}");
        exit(1);
    });
    let services_file = project_dir(project).join("services");

    let mut c = Command::new("bwrap");
    add_container_base(&mut c, &root, project);
    // Make the bags binary available inside as PID 1's program.
    c.arg("--ro-bind").arg(&exe).arg("/run/bags");
    if services_file.exists() {
        c.arg("--ro-bind").arg(&services_file).arg("/run/bags.services");
    }

    // Pipe for bwrap --info-fd: lets us learn the sandbox PID 1's host pid so we can
    // signal it directly for a graceful quiesce. Write end must be inheritable.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!("bags: pipe failed");
        exit(1);
    }
    let (info_r, info_w) = (fds[0], fds[1]);
    unsafe { libc::fcntl(info_r, libc::F_SETFD, libc::FD_CLOEXEC) };

    c.arg("--info-fd").arg(info_w.to_string());
    c.arg("--as-pid-1");
    c.arg("--").arg("/run/bags").arg("init").arg(project);

    install_forwarders();
    eprintln!("[bags] up project={project} (PID-1 supervisor)");
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("bags: failed to exec bwrap: {e} (is bubblewrap installed?)");
            exit(1);
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    unsafe { libc::close(info_w) }; // parent keeps only the read end
    if let Some(p) = read_child_pid(info_r) {
        INIT_PID.store(p, Ordering::SeqCst);
    }
    unsafe { libc::close(info_r) };

    let status = child.wait().unwrap_or_else(|e| {
        eprintln!("bags: wait failed: {e}");
        exit(1);
    });
    exit(status.code().unwrap_or(0));
}

// ---------------------------------------------------------------------------
// bags init — PID 1 supervisor (inside the container)
// ---------------------------------------------------------------------------

struct Service {
    name: String,
    start: Option<String>,
    stop: Option<String>,
}

fn cmd_init(args: &[String]) {
    let project = args.first().map(String::as_str).unwrap_or("?");
    let services = load_services("/run/bags.services");
    let sfd = setup_signalfd();

    let mut running: HashMap<i32, String> = HashMap::new();
    for s in &services {
        if let Some(start) = &s.start {
            match spawn_shell(start) {
                Ok(pid) => {
                    running.insert(pid as i32, s.name.clone());
                    eprintln!("[bags init] started '{}' (pid {pid})", s.name);
                }
                Err(e) => eprintln!("[bags init] failed to start '{}': {e}", s.name),
            }
        }
    }
    let self_pid = unsafe { libc::getpid() };
    eprintln!(
        "[bags init] project={project} pid={self_pid} supervising {} service(s)",
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
            eprintln!("[bags init] ignoring malformed manifest line: {line}");
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
            other => eprintln!("[bags init] unknown manifest keyword '{other}'"),
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

/// Block TERM/INT/CHLD and route them through a signalfd so the supervise loop
/// can't miss a SIGCHLD between reaping and waiting.
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
            eprintln!("[bags init] signalfd failed");
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

/// Spawn a shell command with a clean (unblocked) signal mask — children must
/// not inherit PID 1's blocked signals, or they'd ignore SIGTERM on quiesce.
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
                "[bags init] service '{name}' (pid {pid}) exited: {}",
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

/// The clean quiesce specified in DESIGN.md: stop hooks, warn on undeclared,
/// SIGTERM the tree, wait, then SIGKILL stragglers.
fn quiesce(services: &[Service], running: &HashMap<i32, String>) {
    eprintln!("[bags init] quiescing");
    for s in services {
        if let Some(stop) = &s.stop {
            eprintln!("[bags init] stop hook: '{}'", s.name);
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
            "[bags init] WARNING: {} process(es) beyond declared service entrypoints were running: {list}",
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
    eprintln!("[bags init] stopped cleanly");
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
// stubs + help
// ---------------------------------------------------------------------------

fn stub(cmd: &str) {
    let planned = match cmd {
        "ps" => "list running bags projects and their declared services",
        "move" => "migrate a session to another host (between-turn, over SSH + rsync)",
        "attach" => "attach a roaming client to a running project",
        "preflight" => "report what a move would lose/ship (undeclared procs, host-pins, bytes)",
        _ => "planned",
    };
    eprintln!("bags {cmd}: not implemented yet — planned: {planned}");
    exit(69);
}

fn print_help() {
    println!(
        "bags {VERSION} — ultra-micro containers for coding agents

USAGE:
    bags <command> [args]

COMMANDS:
    run <project> [-- <cmd>]   run a command in a project's namespace          [implemented]
    up <project>               launch a PID-1 supervised container             [implemented]
    ps                         list running projects and services              [planned]
    move <project> <host>      migrate a session to another host               [planned]
    attach <project>           attach a roaming client                         [planned]
    preflight <project>        report the cost/loss of a move                   [planned]
    help | version

Services are declared in ~/.bags/<project>/services, one directive per line:
    start <name> <shell command>
    stop  <name> <shell command>   (optional clean-quiesce hook)

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model."
    );
}
