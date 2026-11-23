//! `tup preflight` / `tup move` — the between-turn migration: quiesce the source, ship the
//! rw root (+ services + net) over rsync/ssh, and bring it up on the target.

use crate::net::read_net_label;
use crate::nixenv;
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
    // Nix env: the closure ships by hash (nix copy / cache), not as raw bytes.
    if nixenv::is_nix_project(project) {
        match nixenv::profile_path(project) {
            Some(p) => eprintln!("  env       : nix — closure ships by hash: {}", p.display()),
            None => eprintln!("  env       : nix — not yet built; target rebuilds from the flake"),
        }
    } else {
        eprintln!("  env       : host /usr (inherited on the target, not shipped)");
    }
    // Persist volumes ride the payload as mutable bytes.
    let persist = project_dir(project).join("persist");
    if persist.exists() {
        let pb = dir_size(&persist);
        eprintln!("  persist   : {} ({pb} bytes)  {}", human(pb), persist.display());
    }
    // Host binds declared in the flake pin the project to paths that must exist on the target.
    let cfg = nixenv::read_tup_config(project);
    if cfg.binds.is_empty() {
        eprintln!("  host-pins : none");
    } else {
        eprintln!(
            "  host-pins : {} bind(s) — won't move unless the target has these paths:",
            cfg.binds.len()
        );
        for b in &cfg.binds {
            eprintln!("              {} {}", if b.ro { "ro" } else { "rw" }, b.src.display());
        }
    }
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

/// Extra `tup up` args so the target resumes with the same env mode as the source.
fn env_up_args(project: &str) -> Vec<String> {
    if nixenv::is_nix_project(project) {
        vec!["--env".to_string(), "nix".to_string()]
    } else {
        Vec::new()
    }
}

/// rsync a sibling payload dir (`persist`, `env`) into another local project dir, if present.
fn ship_dir_local(src_dir: &Path, dst_dir: &Path, name: &str, excludes: &[&str]) {
    let src = src_dir.join(name);
    if !src.exists() {
        return;
    }
    let _ = fs::create_dir_all(dst_dir.join(name));
    let s = format!("{}/", src.display());
    let d = format!("{}/", dst_dir.join(name).display());
    let mut args: Vec<String> = vec!["-a".into(), "--delete".into()];
    for e in excludes {
        args.push("--exclude".into());
        args.push((*e).to_string());
    }
    args.push(s);
    args.push(d);
    run_rsync(&args.iter().map(String::as_str).collect::<Vec<_>>());
}

/// rsync a sibling payload dir to a remote `~/.tup/<project>/<name>`, if present.
fn ship_dir_remote(dest: &str, project: &str, src_dir: &Path, name: &str, excludes: &[&str]) {
    let src = src_dir.join(name);
    if !src.exists() {
        return;
    }
    ssh(dest, &format!("mkdir -p .tup/{project}/{name}"));
    let s = format!("{}/", src.display());
    let d = format!("{dest}:.tup/{project}/{name}/");
    let mut args: Vec<String> = vec!["-a".into(), "--delete".into(), "-e".into(), "ssh".into()];
    for e in excludes {
        args.push("--exclude".into());
        args.push((*e).to_string());
    }
    args.push(s);
    args.push(d);
    run_rsync(&args.iter().map(String::as_str).collect::<Vec<_>>());
}

/// Push the env closure to the target's store by hash — only paths it lacks transfer. The
/// move stays correct without it (the target rebuilds from the flake + its own cache), so a
/// failure is a warning, not a hard error.
fn nix_copy(dest: &str, profile: &Path) {
    eprintln!("[tup] nix copy env closure -> {dest} (by hash; only missing paths transfer)");
    let to = format!("ssh://{dest}");
    let r = Command::new("nix")
        .args([
            "copy",
            "--extra-experimental-features",
            "nix-command flakes",
            "--to",
            &to,
        ])
        .arg(profile)
        .status();
    match r {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("[tup] warning: nix copy failed ({s}); target will rebuild from its cache"),
        Err(e) => {
            eprintln!("[tup] warning: nix unavailable for copy ({e}); target will rebuild from its cache")
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

    // Persist volumes ride the payload; the env flake ships so the target realises the same
    // env by hash (on one machine the closure is already in the shared store).
    ship_dir_local(&src_dir, &dst_dir, "persist", &[]);
    ship_dir_local(&src_dir, &dst_dir, "env", &["profile", "etc"]);

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

    // Bring the project up on the "target" with the SAME net + env mode, and confirm it resumed.
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let log = dst_dir.join("up.log");
    let mut up_args = vec!["up".to_string()];
    up_args.extend(env_up_args(newproj));
    up_args.push("--net".to_string());
    up_args.push(net.clone());
    up_args.push(newproj.to_string());
    spawn_detached(&exe, &up_args, &log);
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

    // Env: ship the flake (so the target realises the same env), and push the built closure by
    // hash via `nix copy` so the target need not rebuild or reach the public cache.
    ship_dir_remote(dest, project, &src_dir, "env", &["profile", "etc"]);
    if nixenv::is_nix_project(project) {
        if let Some(profile) = nixenv::profile_path(project) {
            nix_copy(dest, &profile);
        }
    }
    // Persist volumes ride the payload.
    ship_dir_remote(dest, project, &src_dir, "persist", &[]);

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

    let envflag = if nixenv::is_nix_project(project) {
        "--env nix "
    } else {
        ""
    };
    let up = format!(
        "setsid nohup tup up {envflag}--net {net} {project} > .tup/{project}/up.log 2>&1 < /dev/null &"
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
