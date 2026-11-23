//! `tup run` — one-shot command in a project's namespace.

use crate::net::{net_from_str, port_from_str, Net, PortForward};
use crate::nixenv::{self, env_mode_from_str, EnvMode};
use crate::project::ensure_project_root;
use crate::sandbox::{launch, map_from_str, Bind, Spec, AGENT_SOCK, GATE_DIR};
use crate::{agent, agent::Agent, gate, gate::Gate};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::exit;

pub(crate) fn cmd_run(args: &[String]) {
    let (opts, cmd) = match args.iter().position(|a| a == "--") {
        Some(p) => (&args[..p], args[p + 1..].to_vec()),
        None => (args, Vec::new()),
    };

    let mut net = Net::Pasta;
    let mut net_explicit = false;
    let mut chdir: Option<String> = None;
    let mut harden = false;
    let mut env = EnvMode::Host;
    let mut extra: Vec<Bind> = Vec::new();
    let mut ports: Vec<PortForward> = Vec::new();
    let mut project: Option<String> = None;
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--net" => {
                net = net_from_str(opts.get(i + 1).map(String::as_str));
                net_explicit = true;
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
            "--map" => {
                match opts.get(i + 1) {
                    Some(s) => match map_from_str(s) {
                        Ok(b) => extra.push(b),
                        Err(e) => {
                            eprintln!("tup: --map: {e}");
                            exit(2);
                        }
                    },
                    None => {
                        eprintln!("tup: --map needs HOST:CONTAINER[:ro]");
                        exit(2);
                    }
                }
                i += 2;
            }
            "--port" => {
                match opts.get(i + 1) {
                    Some(s) => match port_from_str(s) {
                        Ok(p) => ports.push(p),
                        Err(e) => {
                            eprintln!("tup: --port: {e}");
                            exit(2);
                        }
                    },
                    None => {
                        eprintln!("tup: --port needs [tcp|udp:]HOST:CONTAINER");
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
            eprintln!("usage: tup run [--net M] [--env host|nix] [--harden] [--bind P] [--ro-bind P] [--map H:C[:ro]] [--port [tcp|udp:]H:C] [--chdir P] <project> [-- <cmd>]");
            exit(2);
        }
    };
    let root = ensure_project_root(&project);
    let chdir = chdir.unwrap_or_else(|| "/work".to_string());
    let mut forward_agent = false;
    let mut agent_handle: Option<Agent> = None;
    let mut gate_handles: Vec<Gate> = Vec::new();
    // Hermetic Nix env: realise the project's flake to a profile store path first (host side,
    // where nix + network + the store live).
    let profile: Option<PathBuf> = match env {
        EnvMode::Nix => {
            // Zero-config: if the prologue selection is still the untouched default, infer it
            // from the working tree (Cargo.toml → rust, package.json → node, …) before building.
            nixenv::auto_detect_prologues(&project);
            let p = nixenv::realize(&project);
            // Apply the flake's `tup` plain-data output: net (unless overridden) + host binds.
            let cfg = nixenv::read_tup_config(&project);
            if !net_explicit {
                if let Some(n) = cfg.net {
                    net = n;
                }
            }
            extra.extend(nixenv::persist_binds(&project, &cfg.persist));
            extra.extend(cfg.binds);
            // Flake-declared port forwards (outputs.tup.ports), merged with any `--port` flags.
            ports.extend(nixenv::read_ports(&project));
            // Credential agent (outputs.tup.agents.ssh): forward the socket, never the key.
            if let Some(name) = &cfg.ssh_agent_key {
                match agent::setup(name, &project) {
                    Ok(a) => {
                        extra.push(Bind {
                            src: a.sock.clone(),
                            dst: PathBuf::from(AGENT_SOCK),
                            ro: false,
                        });
                        forward_agent = true;
                        agent_handle = Some(a);
                    }
                    Err(e) => eprintln!("[tup] warning: ssh agent '{name}' not forwarded: {e}"),
                }
            }
            // Gates (outputs.tup.gates): start each declared host gatekeeper (git built-in +
            // user-defined command gates), then forward the whole gate dir (one <name>.sock each).
            let gates = nixenv::read_gates(&project);
            let mut started_any_gate = false;
            for spec in gates {
                let name = spec.name.clone();
                match gate::spawn(spec, &project, root.clone()) {
                    Ok(g) => {
                        gate_handles.push(g);
                        started_any_gate = true;
                    }
                    Err(e) => eprintln!("[tup] warning: gate '{name}' not started: {e}"),
                }
            }
            if started_any_gate {
                extra.push(Bind {
                    src: gate::gates_dir(&project),
                    dst: PathBuf::from(GATE_DIR),
                    ro: false,
                });
            }
            Some(p)
        }
        EnvMode::Host => None,
    };
    let inner: Vec<OsString> = if cmd.is_empty() {
        nixenv::default_shell(env)
    } else {
        cmd.iter().map(OsString::from).collect()
    };

    // Port forwarding needs pasta's own netns; --net host already shares the host's ports and
    // --net none has no egress. Warn and drop so the Spec reflects what will actually happen.
    if !ports.is_empty() && net != Net::Pasta {
        eprintln!(
            "[tup] warning: ignoring {} --port/flake port forward(s): port forwarding needs --net pasta (have --net {})",
            ports.len(),
            net.label()
        );
        ports.clear();
    }

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
        forward_agent,
        extra,
        ports,
        inner,
    };
    let mut c = launch(spec);
    let result = c.status();
    // Reap the forwarded agent / gatekeeper now the container has exited.
    if let Some(a) = &agent_handle {
        a.stop();
    }
    for g in &gate_handles {
        g.stop();
    }
    match result {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("tup: failed to launch container: {e}");
            exit(1);
        }
    }
}
