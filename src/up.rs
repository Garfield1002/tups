//! `tup up` — launch a PID-1 supervised container (host side) and shepherd its shutdown.

use crate::net::{net_from_str, port_from_str, write_net, Net, PortForward};
use crate::nixenv::{self, env_mode_from_str, EnvMode};
use crate::project::{ensure_project_root, project_dir};
use crate::sandbox::{launch, map_from_str, Bind, Spec, AGENT_SOCK, GATE_SOCK};
use crate::{agent, agent::Agent, gate, gate::Gate};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

static CHILD_PID: AtomicI32 = AtomicI32::new(-1); // direct child (pasta or bwrap), fallback
static INIT_PID: AtomicI32 = AtomicI32::new(-1); // sandbox PID 1, host-visible pid

/// Forward a terminating signal to the sandbox PID 1 directly, so `tup init` runs its
/// quiesce. Never signal the monitor — that triggers pdeathsig SIGKILL and skips quiesce.
extern "C" fn forward_term(_sig: libc::c_int) {
    let p = INIT_PID.load(Ordering::SeqCst);
    let target = if p > 0 {
        p
    } else {
        CHILD_PID.load(Ordering::SeqCst)
    };
    if target > 0 {
        unsafe { libc::kill(target, libc::SIGTERM) };
    }
}

fn install_forwarders() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        let h: extern "C" fn(libc::c_int) = forward_term;
        sa.sa_sigaction = h as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// Does `pid`'s cmdline identify it as *this project's* `tup init`? Guards against a stale
/// `init.pid` whose PID the OS has recycled for an unrelated process — important because
/// `tup stop` sends it a fatal signal.
fn cmdline_is_init(pid: i32, project: &str) -> bool {
    let data = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let parts: Vec<&[u8]> = data.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    parts.len() >= 3
        && parts[0] == b"/run/tup"
        && parts[1] == b"init"
        && parts[2] == project.as_bytes()
}

/// Find the host-visible pid of a project's sandbox PID 1. Prefer the pidfile the Pyrex
/// backend writes (exact, owner-scoped); fall back to scanning /proc for `tup init`'s
/// cmdline (the bwrap backend can't hand us the inner PID 1's host pid directly). Either way
/// the candidate's cmdline is verified, so a stale/recycled PID is never returned.
pub(crate) fn find_init_pid(project: &str) -> Option<i32> {
    if let Ok(s) = fs::read_to_string(project_dir(project).join("init.pid")) {
        if let Ok(p) = s.trim().parse::<i32>() {
            if p > 0 && unsafe { libc::kill(p, 0) } == 0 && cmdline_is_init(p, project) {
                return Some(p);
            }
        }
    }
    let rd = fs::read_dir("/proc").ok()?;
    for e in rd.flatten() {
        let pid = match e.file_name().to_string_lossy().parse::<i32>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if cmdline_is_init(pid, project) {
            return Some(pid);
        }
    }
    None
}

pub(crate) fn poll_init_pid(project: &str, tries: u32) -> Option<i32> {
    for _ in 0..tries {
        if let Some(p) = find_init_pid(project) {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

pub(crate) fn cmd_up(args: &[String]) {
    let mut net = Net::Pasta;
    let mut net_explicit = false;
    let mut harden = false;
    let mut env = EnvMode::Host;
    let mut cli_binds: Vec<Bind> = Vec::new();
    let mut ports: Vec<PortForward> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--net" => {
                net = net_from_str(args.get(i + 1).map(String::as_str));
                net_explicit = true;
                i += 2;
            }
            "--env" => {
                env = env_mode_from_str(args.get(i + 1).map(String::as_str));
                i += 2;
            }
            "--harden" => {
                harden = true;
                i += 1;
            }
            "--bind" | "--ro-bind" => {
                let ro = args[i] == "--ro-bind";
                match args.get(i + 1) {
                    Some(p) => cli_binds.push(Bind {
                        src: p.into(),
                        dst: p.into(),
                        ro,
                    }),
                    None => {
                        eprintln!("tup: {} needs a path", args[i]);
                        exit(2);
                    }
                }
                i += 2;
            }
            "--map" => {
                match args.get(i + 1) {
                    Some(s) => match map_from_str(s) {
                        Ok(b) => cli_binds.push(b),
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
                match args.get(i + 1) {
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
            _ => {
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    if rest.is_empty() {
        eprintln!("usage: tup up [--net pasta|host|none] [--env host|nix] [--harden] [--bind P] [--ro-bind P] [--map H:C[:ro]] [--port [tcp|udp:]H:C] <project>");
        exit(2);
    }
    let project = rest[0].clone();
    let root = ensure_project_root(&project);
    // Hermetic Nix env: realise the flake first, then apply its `tup` output (net + binds).
    let mut tup_binds: Vec<Bind> = Vec::new();
    let mut forward_agent = false;
    let mut agent_handle: Option<Agent> = None;
    let mut gate_handle: Option<Gate> = None;
    let profile = match env {
        EnvMode::Nix => {
            let p = nixenv::realize(&project);
            let cfg = nixenv::read_tup_config(&project);
            if !net_explicit {
                if let Some(n) = cfg.net {
                    net = n;
                }
            }
            tup_binds = nixenv::persist_binds(&project, &cfg.persist);
            tup_binds.extend(cfg.binds);
            // Flake-declared port forwards (outputs.tup.ports), merged with any `--port` flags.
            ports.extend(nixenv::read_ports(&project));
            // Credential agent (outputs.tup.agents.ssh): forward the socket, never the key.
            if let Some(name) = &cfg.ssh_agent_key {
                match agent::setup(name, &project) {
                    Ok(a) => {
                        tup_binds.push(Bind {
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
            // Gate (outputs.tup.gates.git): start the host gatekeeper, forward only its socket.
            if let Some(policy) = nixenv::read_gate(&project) {
                match gate::spawn(policy, &project, root.clone()) {
                    Ok(g) => {
                        tup_binds.push(Bind {
                            src: g.sock.clone(),
                            dst: PathBuf::from(GATE_SOCK),
                            ro: false,
                        });
                        gate_handle = Some(g);
                    }
                    Err(e) => eprintln!("[tup] warning: git gate not started: {e}"),
                }
            }
            Some(p)
        }
        EnvMode::Host => None,
    };
    write_net(&project, net); // so `tup move` restores this net mode, not the pasta default
    let exe = env::current_exe().unwrap_or_else(|e| {
        eprintln!("tup: cannot find own path: {e}");
        exit(1);
    });
    let services_file = project_dir(&project).join("services");

    let mut extra: Vec<Bind> = vec![Bind {
        src: exe,
        dst: PathBuf::from("/run/tup"),
        ro: true,
    }];
    if services_file.exists() {
        extra.push(Bind {
            src: services_file,
            dst: PathBuf::from("/run/tup.services"),
            ro: true,
        });
    }
    extra.extend(tup_binds); // host binds declared in the flake's `tup` output
    extra.extend(cli_binds); // --bind/--ro-bind/--map from the CLI
    // Port forwarding needs pasta's own netns; --net host shares the host's ports already and
    // --net none has no egress. Warn and drop so the Spec reflects what will actually happen.
    if !ports.is_empty() && net != Net::Pasta {
        eprintln!(
            "[tup] warning: ignoring {} --port/flake port forward(s): port forwarding needs --net pasta (have --net {})",
            ports.len(),
            net.label()
        );
        ports.clear();
    }
    let inner: Vec<OsString> = ["/run/tup", "init", &project]
        .iter()
        .map(OsString::from)
        .collect();

    let spec = Spec {
        root,
        project: project.clone(),
        net,
        chdir: "/work".to_string(),
        harden,
        as_pid1: true,
        env,
        profile,
        forward_agent,
        extra,
        ports,
        inner,
    };
    let mut c = launch(spec);
    install_forwarders();
    eprintln!(
        "[tup] up project={project} net={} (PID-1 supervisor){}",
        net.label(),
        if harden { " [hardened]" } else { "" }
    );
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("tup: failed to launch container: {e}");
            // Tear down the host helpers we already started, or they orphan on every failed up.
            if let Some(a) = &agent_handle {
                a.stop();
            }
            if let Some(g) = &gate_handle {
                g.stop();
            }
            exit(1);
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    // Locate the sandbox PID 1 so terminating signals reach it directly (clean quiesce).
    if let Some(p) = poll_init_pid(&project, 100) {
        INIT_PID.store(p, Ordering::SeqCst);
    } else {
        let proj = project.clone();
        std::thread::spawn(move || {
            for _ in 0..600 {
                if INIT_PID.load(Ordering::SeqCst) > 0 {
                    return;
                }
                if let Some(p) = find_init_pid(&proj) {
                    INIT_PID.store(p, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        eprintln!("[tup] init pid not found yet; retrying in background for clean shutdown");
    }
    let status = child.wait().unwrap_or_else(|e| {
        eprintln!("tup: wait failed: {e}");
        if let Some(a) = &agent_handle {
            a.stop();
        }
        if let Some(g) = &gate_handle {
            g.stop();
        }
        exit(1);
    });
    // Reap the forwarded agent / gatekeeper now the supervised container has exited.
    if let Some(a) = &agent_handle {
        a.stop();
    }
    if let Some(g) = &gate_handle {
        g.stop();
    }
    // A child killed by a signal has no exit code; report non-zero, not a false success.
    exit(status.code().unwrap_or(1));
}
