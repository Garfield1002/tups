//! `tup preflight` / `tup move` — the between-turn migration: quiesce the source, ship the
//! rw root (+ services + net) over rsync/ssh, and bring it up on the target.

use crate::net::read_net_label;
use crate::project::project_dir;
use crate::up::{find_init_pid, poll_init_pid};
use std::env;
use std::fs;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::time::Duration;

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
        eprintln!(
            "[preflight] project '{project}' has no root at {}",
            root.display()
        );
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
    eprintln!(
        "  payload   : {} ({bytes} bytes)  {}",
        human(bytes),
        root.display()
    );
    eprintln!("  host-pins : none (link feature not yet implemented)");
    eprintln!("  note      : undeclared processes are reported by the source's quiesce");
}

pub(crate) fn cmd_preflight(args: &[String]) {
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

pub(crate) fn cmd_move(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: tup move <project> <local:<newproject> | [user@]host>");
        exit(2);
    }
    let project = &args[0];
    let dest = &args[1];

    preflight_report(project);

    // Quiesce the source at the (simulated) turn boundary: signal PID 1 directly and wait
    // for the container to tear down, so stop hooks flush state into the root.
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
    if newproj == project {
        eprintln!("tup: source and destination are the same project");
        exit(2);
    }
    // Never rsync --delete over a LIVE destination — that would wipe its working tree out
    // from under a running agent and bring up a second supervisor on the same root.
    if find_init_pid(newproj).is_some() {
        eprintln!("tup: destination '{newproj}' is running — stop it first");
        exit(1);
    }
    let src_dir = project_dir(project);
    let dst_dir = project_dir(newproj);
    let dst_root = dst_dir.join("root");
    let _ = fs::create_dir_all(&dst_root);

    let s = format!("{}/", src_dir.join("root").display());
    let d = format!("{}/", dst_root.display());
    run_rsync(&["-a", "--delete", &s, &d]);

    let src_svc = src_dir.join("services");
    if src_svc.exists() {
        if let Err(e) = fs::copy(&src_svc, dst_dir.join("services")) {
            eprintln!("tup: failed to copy services manifest: {e}");
            exit(1); // don't start the target with an empty service set and report success
        }
    }
    let net = read_net_label(project);
    let _ = fs::copy(src_dir.join("net"), dst_dir.join("net"));
    eprintln!("[tup] shipped root: {project} -> {newproj} (net={net})");

    // Bring the project up on the "target" with the SAME net mode, and confirm it resumed.
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let log = dst_dir.join("up.log");
    spawn_detached(
        &exe,
        &[
            "up".to_string(),
            "--net".to_string(),
            net,
            newproj.to_string(),
        ],
        &log,
    );
    match poll_init_pid(newproj, 150) {
        Some(p) => eprintln!(
            "[tup] resumed as '{newproj}' (init pid {p}); log {}",
            log.display()
        ),
        None => eprintln!(
            "[tup] warning: '{newproj}' did not come up; see {}",
            log.display()
        ),
    }
}

fn remote_running(dest: &str, project: &str) -> bool {
    Command::new("ssh")
        .arg(dest)
        .arg(format!("pgrep -f '/run/tup init {project}' >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn remote_move(project: &str, dest: &str) {
    // Ship to ~/.tup/<project> on the remote (rsync remote paths are relative to $HOME),
    // then start it there detached. Assumes `tup` is on PATH on the remote.
    if remote_running(dest, project) {
        eprintln!("tup: '{project}' is already running on {dest} — stop it there first");
        exit(1);
    }
    let src_dir = project_dir(project);
    let s = format!("{}/", src_dir.join("root").display());
    let remote_root = format!("{dest}:.tup/{project}/root/");

    ssh(dest, &format!("mkdir -p .tup/{project}/root"));
    run_rsync(&["-a", "--delete", "-e", "ssh", &s, &remote_root]);

    let src_svc = src_dir.join("services");
    if src_svc.exists() {
        if let Some(svc) = src_svc.to_str() {
            run_rsync(&[
                "-a",
                "-e",
                "ssh",
                svc,
                &format!("{dest}:.tup/{project}/services"),
            ]);
        }
    }
    let net = read_net_label(project);
    let netf = src_dir.join("net");
    if netf.exists() {
        if let Some(nf) = netf.to_str() {
            run_rsync(&["-a", "-e", "ssh", nf, &format!("{dest}:.tup/{project}/net")]);
        }
    }
    eprintln!("[tup] shipped root: {project} -> {dest} (net={net})");

    let up = format!(
        "setsid nohup tup up --net {net} {project} > .tup/{project}/up.log 2>&1 < /dev/null &"
    );
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
