//! tup — ultra-micro containers for coding agents.
//!
//! Contain the blast radius of a coding agent (dedicated fs/pid/ipc/uts + per-project
//! netns), and move its session between servers between turns. This file is just the verb
//! dispatch; each component lives in its own module:
//!
//!   project   project identity, state dir, name validation
//!   net       per-project network mode + the pasta DNS relay
//!   sandbox   the backend-neutral container Spec + mount model + backend seam
//!   bwrap     default backend: render a Spec into a bubblewrap invocation
//!   pyrex       optional backend (feature "pyrex"): realise a Spec with raw namespace syscalls
//!   run       `tup run`  — one-shot command
//!   up        `tup up`   — launch + shepherd a PID-1 supervised container
//!   init      `tup init` — the in-container PID 1 / supervisor
//!   migrate   `tup move` / `tup preflight` — the between-turn migration

use std::env;
use std::process::exit;

mod bwrap;
#[cfg(feature = "pyrex")]
mod pyrex;
mod init;
mod migrate;
mod net;
mod project;
mod run;
mod sandbox;
mod up;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("help") {
        "run" => run::cmd_run(&args[2..]),
        "up" => up::cmd_up(&args[2..]),
        "init" => init::cmd_init(&args[2..]),
        "move" => migrate::cmd_move(&args[2..]),
        "preflight" => migrate::cmd_preflight(&args[2..]),
        // Internal: the pyrex backend re-execs the tup binary as the sandbox launcher.
        "__sandbox" => cmd_sandbox(&args[2..]),
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

#[cfg(feature = "pyrex")]
fn cmd_sandbox(args: &[String]) -> ! {
    pyrex::run_sandbox(args)
}

#[cfg(not(feature = "pyrex"))]
fn cmd_sandbox(_args: &[String]) -> ! {
    eprintln!("tup: __sandbox requires a build with the 'pyrex' feature");
    exit(2)
}

fn stub(cmd: &str) {
    let planned = match cmd {
        "ps" => "list running tup projects and their declared services",
        "attach" => "attach a roaming client to a running project",
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
    run [--net M] [--harden] <project> [-- <cmd>]   run a command in a namespace  [implemented]
    up  [--net M] [--harden] <project>              launch a PID-1 supervisor      [implemented]
    ps                                              list running projects          [planned]
    move <project> <dest>                           migrate a session              [implemented]
    attach <project>                                attach a roaming client        [planned]
    preflight <project>                             report the cost/loss of a move [implemented]
    help | version

    --net pasta  (default) dedicated netns, own loopback, NAT egress via pasta
    --net host   share the host network namespace
    --net none   isolated netns, loopback only, no egress
    --harden     drop all capabilities (--cap-drop ALL) + new session (TTY-injection proof).
                 We don't advertise security; this just lowers the escape stakes.

BACKENDS:
    Default: bubblewrap. A build with `--features pyrex` also compiles an in-house
    namespace backend, selected at runtime with TUP_SANDBOX=pyrex (default: bwrap).

Services are declared in ~/.tup/<project>/services, one directive per line:
    start <name> <shell command>
    stop  <name> <shell command>   (optional clean-quiesce hook)

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model."
    );
}
