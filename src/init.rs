//! `tup init` — the in-container PID 1 / supervisor. Starts declared services, reaps the
//! whole process tree, and on a terminating signal runs stop hooks before a bounded
//! SIGTERM→SIGKILL sweep (the clean quiesce a move depends on).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::{exit, Command};
use std::time::Duration;

struct Service {
    name: String,
    start: Option<String>,
    stop: Option<String>,
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
