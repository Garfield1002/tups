//! tup — ultra-micro containers for coding agents.
//!
//! Contain the blast radius of a coding agent (dedicated fs/pid/ipc/uts + per-project
//! netns), and move its session between servers between turns. This file is just the verb
//! dispatch; each component lives in its own module:
//!
//!   project   project identity, state dir, name validation
//!   net       per-project network mode + the pasta DNS relay
//!   nixenv    hermetic Nix env: eval the project flake → profile → PATH is just that
//!   sandbox   the backend-neutral container Spec + mount model + backend seam
//!   bwrap     default backend: render a Spec into a bubblewrap invocation
//!   pyrex       optional backend (feature "pyrex"): realise a Spec with raw namespace syscalls
//!   run       `tup run`  — one-shot command
//!   up        `tup up`   — launch + shepherd a PID-1 supervised container
//!   status    `tup ps` / `tup attach` — list projects; join a running container
//!   init      `tup init` — the in-container PID 1 / supervisor
//!   migrate   `tup move` / `tup preflight` — the between-turn migration

use std::env;
use std::ffi::OsString;
use std::process::exit;

mod agent;
mod bwrap;
#[cfg(feature = "pyrex")]
mod pyrex;
mod init;
mod migrate;
mod net;
mod nixenv;
mod project;
mod run;
mod sandbox;
mod status;
mod up;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // Collect as OsString: env::args() panics on a non-UTF-8 argument, and the pyrex backend
    // re-execs `tup __sandbox …` carrying paths that must survive verbatim. The String-based
    // verbs below take a lossy conversion (their inputs — names, flags — are ASCII anyway).
    let args: Vec<OsString> = env::args_os().collect();
    let verb = args
        .get(1)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "help".to_string());
    // Internal: the pyrex backend re-execs the tup binary as the sandbox launcher. It needs the
    // original OsStrings, so it is dispatched before the lossy String conversion.
    if verb == "__sandbox" {
        cmd_sandbox(&args[2..]);
    }
    let rest: Vec<String> = args
        .iter()
        .skip(2)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    match verb.as_str() {
        "run" => run::cmd_run(&rest),
        "up" => up::cmd_up(&rest),
        "env" => nixenv::cmd_env(&rest),
        "init" => init::cmd_init(&rest),
        "move" => migrate::cmd_move(&rest),
        "preflight" => migrate::cmd_preflight(&rest),
        "ps" => status::cmd_ps(&rest),
        "attach" => status::cmd_attach(&rest),
        "stop" => status::cmd_stop(&rest),
        "rm" => status::cmd_rm(&rest),
        "version" | "--version" | "-V" => println!("tup {VERSION}"),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("tup: unknown command '{other}'\n");
            print_help();
            exit(2);
        }
    }
}

#[cfg(feature = "pyrex")]
fn cmd_sandbox(args: &[OsString]) -> ! {
    pyrex::run_sandbox(args)
}

#[cfg(not(feature = "pyrex"))]
fn cmd_sandbox(_args: &[OsString]) -> ! {
    eprintln!("tup: __sandbox requires a build with the 'pyrex' feature");
    exit(2)
}

fn print_help() {
    println!(
        "tup {VERSION} — ultra-micro containers for coding agents

USAGE:
    tup <command> [args]

COMMANDS:
    run [--net M] [--env E] [--harden] <project> [-- <cmd>]   run in a namespace   [implemented]
    up  [--net M] [--env E] [--harden] <project>              PID-1 supervisor     [implemented]
    env <project> [list|add <p>|rm <p>]             inspect/edit env prologues     [implemented]
    ps                                              list projects and their state  [implemented]
    stop <project>                                  clean-stop a running project   [implemented]
    rm [--force] <project>                          delete a project's state       [implemented]
    move <project> <dest>                           migrate a session              [implemented]
    attach <project> [-- <cmd>]                     join a running container       [implemented]
    preflight <project>                             report the cost/loss of a move [implemented]
    help | version

    --net pasta  (default) dedicated netns, own loopback, NAT egress via pasta
    --net host   share the host network namespace
    --net none   isolated netns, loopback only, no egress
    --env host   (default) inherit the host /usr + /etc
    --env nix    hermetic: the project's flake IS the env; drop host /usr, PATH is the
                 profile's bin. Composed from prologues (base + opt-in languages); the
                 flake's `tup` output supplies net + host binds. Needs nix on PATH.
    --harden     drop all capabilities (--cap-drop ALL) + new session (TTY-injection proof).
                 We don't advertise security; this just lowers the escape stakes.

BACKENDS:
    Default: pyrex, an in-house raw-namespace backend. Set TUP_SANDBOX=bwrap to use
    bubblewrap instead, or build with `--no-default-features` for a bwrap-only binary.

Services are declared in ~/.tup/<project>/services, one directive per line:
    start <name> <shell command>
    stop  <name> <shell command>   (optional clean-quiesce hook)

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model."
    );
}
