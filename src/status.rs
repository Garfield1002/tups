//! `tup ps` — list projects and their state. `tup attach` — join a running container.
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
use std::path::PathBuf;
use std::process::{exit, Command};

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
        "{:<20} {:<8} {:<8} {:<6} {:<5} {}",
        "PROJECT", "STATE", "PID", "NET", "ENV", "SVCS"
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
