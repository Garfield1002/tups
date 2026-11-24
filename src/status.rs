//! Project lifecycle and inspection: `tup ps` (list), `tup attach` (join a running
//! container), `tup stop` (clean shutdown), `tup rm` (delete project state).
//!
//! `attach` is the roaming-client reconnect: it `nsenter`s into the live container's
//! namespaces (user first, so we gain caps in it, then mount/uts/ipc/pid/net) and runs a shell
//! as the container's PID-1 world sees it. No host daemon — just the kernel's `setns`.

use crate::net::read_net_label;
use crate::nixenv;
use crate::project::project_dir;
use crate::up::find_init_pid;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::time::Duration;

fn tup_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".tup")
}

/// Count declared services (distinct names with a `start` directive).
fn service_count(project: &str) -> usize {
    let f = project_dir(project).join("services");
    let content = match fs::read_to_string(&f) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let mut names = BTreeSet::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        if it.next() == Some("start") {
            if let Some(name) = it.next() {
                names.insert(name.to_string());
            }
        }
    }
    names.len()
}

pub(crate) fn cmd_ps(_args: &[String]) {
    let home = tup_home();
    let mut rows: Vec<(String, Option<i32>, String, bool, usize)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&home) {
        for e in rd.flatten() {
            // A project is a dir with a `root/` working tree.
            if !e.path().join("root").is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let pid = find_init_pid(&name);
            // Only show a net mode the project actually recorded (via `tup up`); a never-upped
            // project has no `net` file, so show "-" rather than the misleading pasta default.
            let net = if project_dir(&name).join("net").exists() {
                read_net_label(&name)
            } else {
                "-".to_string()
            };
            let nix = nixenv::is_nix_project(&name);
            let svcs = service_count(&name);
            rows.push((name, pid, net, nix, svcs));
        }
    }
    if rows.is_empty() {
        println!("no projects under {}", home.display());
        return;
    }
    // Running first, then alphabetical.
    rows.sort_by(|a, b| b.1.is_some().cmp(&a.1.is_some()).then(a.0.cmp(&b.0)));
    println!(
        "{:<20} {:<8} {:<8} {:<6} {:<5} SVCS",
        "PROJECT", "STATE", "PID", "NET", "ENV"
    );
    for (name, pid, net, nix, svcs) in rows {
        println!(
            "{:<20} {:<8} {:<8} {:<6} {:<5} {}",
            name,
            if pid.is_some() { "running" } else { "stopped" },
            pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            net,
            if nix { "nix" } else { "host" },
            svcs
        );
    }
}

pub(crate) fn cmd_attach(args: &[String]) {
    let (opts, cmd) = match args.iter().position(|a| a == "--") {
        Some(p) => (&args[..p], args[p + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let project = match opts.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: tup attach <project> [-- <cmd>]");
            exit(2);
        }
    };
    let pid = match find_init_pid(&project) {
        Some(p) => p,
        None => {
            eprintln!("tup: '{project}' is not running — nothing to attach to (see `tup ps`)");
            exit(1);
        }
    };
    let shell: Vec<OsString> = if cmd.is_empty() {
        vec![OsString::from("/bin/sh")]
    } else {
        cmd.iter().map(OsString::from).collect()
    };

    // The inherited host env is wrong inside the container (a hermetic root has no host /usr on
    // PATH at all), so give the shell a clean env built for *this* container, via the
    // container's own `/usr/bin/env -i` (present in both host and hermetic roots). We don't
    // clear nsenter's own env — Command needs the host PATH to locate the nsenter binary.
    let path = match nixenv::profile_path(&project) {
        Some(p) if nixenv::is_nix_project(&project) => format!("{}/bin", p.display()),
        _ => "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
    };
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm".into());

    eprintln!("[tup] attaching to '{project}' (init pid {pid})");
    let mut c = Command::new("nsenter");
    // --preserve-credentials: don't reset uid/gid/groups (the container's userns has
    // setgroups=deny, and our host uid already maps to container-root). --user first so we hold
    // caps in that userns; then its other namespaces. --wdns resolves the cwd inside the mount ns.
    c.arg(format!("--target={pid}")).args([
        "--preserve-credentials",
        "--user",
        "--mount",
        "--uts",
        "--ipc",
        "--pid",
    ]);
    // Networking: a pasta container's netns is owned by pasta's *outer* userns, which we don't
    // hold caps in once we've entered the container's inner userns — so setns into it would
    // EPERM. Skip it and let the shell share the host network (cooperative model), with a note.
    // For host/none the netns is reachable, so enter it.
    if read_net_label(&project) == "pasta" {
        eprintln!("[tup] note: pasta netns isn't enterable from the container userns; the attached shell uses the host network");
    } else {
        c.arg("--net");
    }
    c.arg("--wdns=/work");
    c.arg("--")
        .arg("/usr/bin/env")
        .arg("-i")
        .arg(format!("PATH={path}"))
        .arg("HOME=/work")
        .arg(format!("TERM={term}"))
        .arg(format!("TUP_PROJECT={project}"))
        .args(&shell);
    match c.status() {
        Ok(s) => exit(s.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("tup: nsenter failed: {e} (is util-linux installed?)");
            exit(1);
        }
    }
}

/// `tup logs <project> [-f]` — print a session's log (like `docker logs`).
///
/// A detached `tup up`/`tup move` session (and the services its PID-1 supervisor shepherds)
/// writes `~/.tup/<project>/up.log`. A *foreground* `tup run`/`tup up` writes to your terminal,
/// not a file, so there is no log to show. `-f` follows the file by polling for appended bytes
/// (Ctrl-C to stop).
pub(crate) fn cmd_logs(args: &[String]) {
    use std::io::{Read, Write};

    let mut follow = false;
    let mut name: Option<String> = None;
    for a in args {
        match a.as_str() {
            "-f" | "--follow" => follow = true,
            _ if name.is_none() => name = Some(a.clone()),
            _ => {
                eprintln!("tup: unexpected argument '{a}'");
                exit(2);
            }
        }
    }
    let project = match name {
        Some(p) => p,
        None => {
            eprintln!("usage: tup logs [-f] <project>");
            exit(2);
        }
    };
    crate::project::ensure_valid_name(&project);
    let log = project_dir(&project).join("up.log");
    let mut f = match fs::File::open(&log) {
        Ok(f) => f,
        Err(_) => {
            println!(
                "no log for '{project}' ({}).\n\
                 a detached `tup up`/`tup move` session writes up.log; a foreground \
                 `tup run`/`tup up` writes to your terminal, not a file.",
                log.display()
            );
            return;
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 8192];
    loop {
        match f.read(&mut buf) {
            Ok(0) => {
                // Reached the current end of the file.
                if !follow {
                    break;
                }
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    break; // pipe closed (e.g. `tup logs … | head`)
                }
            }
            Err(e) => {
                eprintln!("tup: read {}: {e}", log.display());
                exit(1);
            }
        }
    }
    let _ = out.flush();
}

/// `tup stop <project>` — cleanly stop a running project. We SIGTERM the sandbox PID 1 so
/// `tup init` runs its quiesce (stop hooks, then the bounded SIGTERM→SIGKILL sweep) — never
/// the monitor, whose pdeathsig would SIGKILL the tree and skip quiesce. Then we wait for
/// PID 1 to exit. Idempotent: stopping an already-stopped project succeeds with a note.
pub(crate) fn cmd_stop(args: &[String]) {
    let project = match args.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: tup stop <project>");
            exit(2);
        }
    };
    crate::project::ensure_valid_name(&project);
    let pid = match find_init_pid(&project) {
        Some(p) => p,
        None => {
            println!("'{project}' is not running");
            return;
        }
    };
    eprintln!("[tup] stopping '{project}' (init pid {pid}) — running quiesce");
    // SIGTERM to PID 1; it runs stop hooks and the sweep, then exits and disappears host-side.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    // Poll via find_init_pid (which re-verifies the cmdline), never a bare kill(pid, 0): once
    // init exits, the OS may recycle its PID, and we must not then SIGKILL the innocent reuser.
    for _ in 0..150 {
        if find_init_pid(&project) != Some(pid) {
            println!("stopped '{project}'");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("[tup] '{project}' still running after 15s; sending SIGKILL (quiesce may be incomplete)");
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// `tup rm [--force] <project>` — delete a project's state dir (working tree + persist
/// volumes + env). Safe by default: without `--force` it only reports what would be deleted,
/// since `root/` holds the user's actual work. Refuses outright while the project is running.
pub(crate) fn cmd_rm(args: &[String]) {
    let mut force = false;
    let mut name: Option<String> = None;
    for a in args {
        match a.as_str() {
            "--force" | "-f" | "--yes" | "-y" => force = true,
            _ if name.is_none() => name = Some(a.clone()),
            _ => {}
        }
    }
    let project = match name {
        Some(p) => p,
        None => {
            eprintln!("usage: tup rm [--force] <project>");
            exit(2);
        }
    };
    // Validate BEFORE deriving a path: `tup rm ..` must never resolve to ~/.tup/.. (= $HOME)
    // and be handed to remove_dir_all.
    crate::project::ensure_valid_name(&project);
    let dir = project_dir(&project);
    if !dir.is_dir() {
        eprintln!("tup: no such project '{project}' (nothing under {})", dir.display());
        exit(1);
    }
    if let Some(pid) = find_init_pid(&project) {
        eprintln!("tup: '{project}' is running (init pid {pid}) — run `tup stop {project}` first");
        exit(1);
    }
    let bytes = dir_size(&dir);
    if !force {
        println!("tup rm would permanently delete '{project}':");
        println!("  {}  ({})", dir.display(), human_bytes(bytes));
        println!("this includes the working tree (root/) and any persist volumes.");
        println!("re-run with --force to delete.");
        return;
    }
    match fs::remove_dir_all(&dir) {
        Ok(_) => println!("removed '{project}' ({})", human_bytes(bytes)),
        Err(e) => {
            eprintln!("tup: failed to remove {}: {e}", dir.display());
            exit(1);
        }
    }
}

/// Sum of regular-file sizes under `p` (recursive; symlinks counted as their own small entry,
/// never followed — so a persist bind-target outside the tree isn't double-counted or chased).
fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                total += dir_size(&e.path());
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < UNITS.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{b:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
