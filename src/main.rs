//! bags — ultra-micro containers for coding agents.
//!
//! Increment 1: `bags run <project> -- <cmd>` assembles a namespace (shared read-only
//! system layer + a dedicated read-write project root + a PID namespace) via bubblewrap
//! and runs a command inside it. Everything else is stubbed with its planned behavior.
//!
//! Slice-1 shortcuts (see DESIGN.md "Increments"):
//!   - shared host rootfs is bind-mounted read-only instead of a dedicated overlay root
//!     (dedicated root/overlay = increment 4)
//!   - host networking is shared instead of a dedicated netns (pasta netns = increment 3)
//!   - the command runs directly, not under a PID-1 supervisor (bags init = increment 2)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("help") {
        "run" => cmd_run(&args[2..]),
        "up" | "init" | "ps" | "move" | "attach" | "preflight" => stub(&args[1]),
        "version" | "--version" | "-V" => println!("bags {VERSION}"),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("bags: unknown command '{other}'\n");
            print_help();
            exit(2);
        }
    }
}

/// `bags run <project> [-- <cmd...>]`
fn cmd_run(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: bags run <project> [-- <cmd...>]");
        exit(2);
    }
    let project = &args[0];
    let rest = &args[1..];

    // Command after `--`; default to an interactive shell.
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
    c.args([
        // Isolation: user, pid, ipc, uts namespaces. (net = increment 3; dedicated root = 4.)
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
    ]);
    // Shared read-only system layer, assembled on a fresh root so we can still create
    // our own mountpoints (binding `/` ro would make the root unwritable). This is also
    // the shape the real dedicated-root/overlay (increment 4) grows from.
    add_system_layer(&mut c);
    // Dedicated read-write project root.
    c.arg("--bind").arg(&root).arg("/work");
    // Fresh virtual filesystems so the container has its own /proc, /dev, /tmp.
    c.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    c.args(["--chdir", "/work"]);
    c.args(["--setenv", "BAGS_PROJECT", project]);
    c.arg("--");
    c.args(&cmd);

    eprintln!(
        "[bags] project={project} root={} (slice-1: shared rootfs ro, host net, no supervisor)",
        root.display()
    );

    match c.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("bags: failed to exec bwrap: {e}");
            eprintln!("      (is bubblewrap installed and on PATH?)");
            exit(1);
        }
    }
}

/// Bind the shared, read-only system layer onto a fresh container root.
///
/// `/usr` and `/etc` are the substance. The top-level `/bin`, `/lib`, … are usually
/// symlinks into `/usr` on merged-`/usr` systems (Debian/Arch/Fedora); we replicate the
/// symlink so dynamic loaders resolve. The shared `/nix/store` — the real point of bags —
/// is bound when present.
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

fn ensure_project_root(project: &str) -> PathBuf {
    if project.is_empty() || project.contains('/') || project == "." || project == ".." {
        eprintln!("bags: invalid project name '{project}'");
        exit(2);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let root = PathBuf::from(home).join(".bags").join(project).join("root");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("bags: cannot create {}: {e}", root.display());
        exit(1);
    }
    root
}

fn stub(cmd: &str) {
    let planned = match cmd {
        "up" => "bring up a project container (PID 1 supervisor + declared services)",
        "init" => "in-container PID 1 / supervisor (started by `up`, not called directly)",
        "ps" => "list running bags projects and their declared services",
        "move" => "migrate a session to another host (between-turn, over SSH + rsync)",
        "attach" => "attach a roaming client to a running project",
        "preflight" => "report what a move would lose/ship (undeclared procs, host-pins, bytes)",
        _ => "planned",
    };
    eprintln!("bags {cmd}: not implemented yet — planned: {planned}");
    exit(69); // EX_UNAVAILABLE
}

fn print_help() {
    println!(
        "bags {VERSION} — ultra-micro containers for coding agents

USAGE:
    bags <command> [args]

COMMANDS:
    run <project> [-- <cmd>]   run a command in a project's isolated namespace   [implemented]
    up <project>               bring up a container (PID 1 supervisor)           [planned]
    ps                         list running projects and services               [planned]
    move <project> <host>      migrate a session to another host                [planned]
    attach <project>           attach a roaming client                          [planned]
    preflight <project>        report the cost/loss of a move                    [planned]
    help | version

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model."
    );
}
