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
mod copyfile;
mod detect;
mod gate;
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
        "inspect" => status::cmd_inspect(&rest),
        "attach" => status::cmd_attach(&rest),
        "logs" => status::cmd_logs(&rest),
        "cp" => copyfile::cmd_cp(&rest),
        "stop" => status::cmd_stop(&rest),
        "rm" => status::cmd_rm(&rest),
        // Container-side git-gate aliases: ask the host gatekeeper to run the filtered op.
        "push" => gate::drive_git("push", &rest),
        "fetch" => gate::drive_git("fetch", &rest),
        // `tup gate <name> [args]` drives any named gate (container side); `tup gate serve`
        // runs a standalone git gatekeeper (host side, bootstrap / non-flake use).
        "gate" => gate::cmd_gate(&rest),
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
    run [flags] <project> [-- <cmd>]                run one command in a namespace
    up  [flags] <project>                           PID-1 supervisor (declared services)
    env <project> [list|add <p>|rm <p>|detect]      inspect/edit env prologues (detect: infer)
    ps                                              list projects and their state
    inspect <project>                               dump a project's resolved config
    logs [-f] <project>                             show a session's up.log (-f: follow)
    cp <src> <dst>                                  copy files host<->project tree (<project>:<path>)
    attach <project> [-- <cmd>]                     join a running container
    stop <project>                                  clean-stop a running project
    rm [--force] <project>                          delete a project's state
    move [--ship-all] <project> <local:<p>|host>    migrate a session
    preflight <project>                             report the cost/loss of a move
    push [<ref>] / fetch [<ref>]                    drive the git gate (host does the op)
    gate <name> [args…]                             drive a named gate (git or user-defined command)
    gate serve --remote U --allow push --sock P [--tree D] [--refs R]   standalone gatekeeper
    help | version

RUN/UP FLAGS:
    --net pasta|host|none   pasta (default): own netns + NAT egress; host: share; none: loopback-only
    --env host|nix          host (default): inherit /usr+/etc; nix: hermetic flake env (auto-detects lang)
    --bind P / --ro-bind P  bind a host path at the same container path (rw / ro)
    --map HOST:CONTAINER[:ro]   bind a host path at a different container path
    --port [tcp|udp:]H:C    forward host port H to container port C (pasta only)
    -e KEY=VAL              inject an environment variable (repeatable)
    -w, --workdir P         working directory inside the container (run also: --chdir)
    --harden                --cap-drop ALL + new session (lowers escape stakes; not a sandbox)

The hermetic env is the project's flake (`~/.tup/<p>/env/flake.nix`): host /usr is dropped and
PATH is just the profile's bin. A fresh Rust/Python/JS project is auto-detected and the matching
prologue activated. The flake's `outputs.tup` carries net, binds, ports, persist, gates, and the
move ship/rebuild dial as plain data.

BACKENDS:
    Default: pyrex, an in-house raw-namespace backend. Set TUP_SANDBOX=bwrap to use
    bubblewrap instead, or build with `--no-default-features` for a bwrap-only binary.

Services are declared in ~/.tup/<project>/services, one directive per line:
    start   <name> <shell command>
    stop    <name> <shell command>          (optional clean-quiesce hook)
    restart <name> no|on-failure|always     (optional; default no)

Contain the blast radius of a coding agent; move its session between servers.
See DESIGN.md for the full model, DOCKER_PARITY.md for the Docker mapping."
    );
}
