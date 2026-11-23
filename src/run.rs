//! `tup run` — one-shot command in a project's namespace.

use crate::net::{net_from_str, Net};
use crate::nixenv::{self, env_mode_from_str, EnvMode};
use crate::project::ensure_project_root;
use crate::sandbox::{launch, Bind, Spec};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::exit;

pub(crate) fn cmd_run(args: &[String]) {
    let (opts, cmd) = match args.iter().position(|a| a == "--") {
        Some(p) => (&args[..p], args[p + 1..].to_vec()),
        None => (args, Vec::new()),
    };

    let mut net = Net::Pasta;
    let mut chdir: Option<String> = None;
    let mut harden = false;
    let mut env = EnvMode::Host;
    let mut extra: Vec<Bind> = Vec::new();
    let mut project: Option<String> = None;
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--net" => {
                net = net_from_str(opts.get(i + 1).map(String::as_str));
                i += 2;
            }
            "--env" => {
                env = env_mode_from_str(opts.get(i + 1).map(String::as_str));
                i += 2;
            }
            "--harden" => {
                harden = true;
                i += 1;
            }
            "--bind" | "--ro-bind" => {
                let ro = opts[i] == "--ro-bind";
                match opts.get(i + 1) {
                    Some(p) => extra.push(Bind {
                        src: p.into(),
                        dst: p.into(),
                        ro,
                    }),
                    None => {
                        eprintln!("tup: {} needs a path", opts[i]);
                        exit(2);
                    }
                }
                i += 2;
            }
            "--chdir" => {
                chdir = opts.get(i + 1).cloned();
                if chdir.is_none() {
                    eprintln!("tup: --chdir needs a path");
                    exit(2);
                }
                i += 2;
            }
            _ => {
                if project.is_none() {
                    project = Some(opts[i].clone());
                } else {
                    eprintln!("tup: unexpected argument '{}'", opts[i]);
                    exit(2);
                }
                i += 1;
            }
        }
    }
    let project = match project {
        Some(p) => p,
        None => {
            eprintln!("usage: tup run [--net M] [--env host|nix] [--harden] [--bind P] [--ro-bind P] [--chdir P] <project> [-- <cmd>]");
            exit(2);
        }
    };
    let root = ensure_project_root(&project);
    let chdir = chdir.unwrap_or_else(|| "/work".to_string());
    // Hermetic Nix env: realise the project's flake to a profile store path first (host side,
    // where nix + network + the store live).
    let profile: Option<PathBuf> = match env {
        EnvMode::Nix => Some(nixenv::realize(&project)),
        EnvMode::Host => None,
    };
    let inner: Vec<OsString> = if cmd.is_empty() {
        nixenv::default_shell(env)
    } else {
        cmd.iter().map(OsString::from).collect()
    };

    eprintln!(
        "[tup] run project={project} net={} env={} chdir={chdir}{}",
        net.label(),
        env.label(),
        if harden { " [hardened]" } else { "" }
    );
    let spec = Spec {
        root,
        project,
        net,
        chdir,
        harden,
        as_pid1: false,
        env,
        profile,
        extra,
        inner,
    };
    let mut c = launch(spec);
    match c.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("tup: failed to launch container: {e}");
            exit(1);
        }
    }
}
