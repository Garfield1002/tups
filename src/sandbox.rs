//! The container spec and the backend seam.
//!
//! A `Spec` describes *what* container to build; a backend decides *how*. The system
//! layer is expressed once, as a backend-neutral list of `Op`s (`base_ops`), so the
//! `bwrap` backend (which renders them to bwrap flags) and the `pyrex` backend (which
//! renders them to raw mount syscalls) can never drift apart.
//!
//! Networking: for `Net::Pasta` the launcher is wrapped in pasta (outer netns + NAT
//! egress + DNS forward); the backend's own process runs *inside* pasta's namespaces.

use crate::net::{ensure_resolv, Net, PASTA_DNS};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A caller-supplied bind (the project tree, ~/.claude, the exe at /run/tup, …).
pub(crate) struct Bind {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub ro: bool,
}

/// One filesystem operation to realise inside the container's mount namespace, in order.
pub(crate) enum Op {
    Bind { src: PathBuf, dst: PathBuf, ro: bool },
    Symlink { target: PathBuf, dst: PathBuf },
    Proc(PathBuf),
    Dev(PathBuf),
    Tmpfs(PathBuf),
}

/// Everything needed to launch one container, independent of the backend.
pub(crate) struct Spec {
    pub root: PathBuf,
    pub project: String,
    pub net: Net,
    pub chdir: String,
    pub harden: bool,
    /// Run the inner command as the container's PID 1 (what `tup up`'s supervisor wants).
    pub as_pid1: bool,
    pub extra: Vec<Bind>,
    pub inner: Vec<OsString>,
}

/// The shared read-only system layer plus the project's rw root and virtual filesystems,
/// as backend-neutral ops. Applied in order.
pub(crate) fn base_ops(spec: &Spec) -> Vec<Op> {
    let mut v: Vec<Op> = Vec::new();
    // Shared read-only system layer on a fresh root (so we can create our own mountpoints).
    for p in ["/usr", "/etc"] {
        if Path::new(p).exists() {
            v.push(Op::Bind {
                src: p.into(),
                dst: p.into(),
                ro: true,
            });
        }
    }
    // Merged-usr symlinks (/bin -> usr/bin, …) are symlinks on the host; recreate them.
    for name in ["bin", "sbin", "lib", "lib64", "libx32", "lib32"] {
        let p = PathBuf::from(format!("/{name}"));
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                if let Ok(target) = std::fs::read_link(&p) {
                    v.push(Op::Symlink { target, dst: p });
                }
            }
            Ok(_) => v.push(Op::Bind {
                src: p.clone(),
                dst: p,
                ro: true,
            }),
            Err(_) => {}
        }
    }
    if Path::new("/nix").exists() {
        v.push(Op::Bind {
            src: "/nix".into(),
            dst: "/nix".into(),
            ro: true,
        });
    }
    // Dedicated read-write project root.
    v.push(Op::Bind {
        src: spec.root.clone(),
        dst: "/work".into(),
        ro: false,
    });
    v.push(Op::Proc("/proc".into()));
    v.push(Op::Dev("/dev".into()));
    v.push(Op::Tmpfs("/tmp".into()));
    // Caller-supplied binds.
    for b in &spec.extra {
        v.push(Op::Bind {
            src: b.src.clone(),
            dst: b.dst.clone(),
            ro: b.ro,
        });
    }
    // DNS relay for the isolated netns (pasta only).
    if spec.net == Net::Pasta {
        let resolv = ensure_resolv(&spec.project);
        let target = std::fs::canonicalize("/etc/resolv.conf")
            .unwrap_or_else(|_| PathBuf::from("/etc/resolv.conf"));
        v.push(Op::Bind {
            src: resolv,
            dst: target,
            ro: true,
        });
    }
    v
}

/// Environment variables every container gets.
pub(crate) fn env_pairs(spec: &Spec) -> Vec<(&'static str, String)> {
    vec![
        ("TUP_PROJECT", spec.project.clone()),
        ("TUP_NET", spec.net.label().to_string()),
    ]
}

/// Wrap a launcher argv in pasta when `Net::Pasta`; otherwise run it directly.
pub(crate) fn wrap_pasta(net: Net, launcher: &str, args: &[OsString]) -> Command {
    match net {
        Net::Pasta => {
            let mut c = Command::new("pasta");
            c.arg("--config-net")
                .arg("--dns-forward")
                .arg(PASTA_DNS)
                .arg("--quiet");
            c.arg("--").arg(launcher).args(args);
            c
        }
        Net::Host | Net::None => {
            let mut c = Command::new(launcher);
            c.args(args);
            c
        }
    }
}

/// Build the launch `Command`, selecting the backend. The `pyrex` backend is compiled in
/// only under its feature and is still opt-in at runtime via `TUP_SANDBOX=pyrex`, so the
/// default path is always the proven bwrap backend.
pub(crate) fn launch(spec: Spec) -> Command {
    #[cfg(feature = "pyrex")]
    {
        if crate::pyrex::selected() {
            return crate::pyrex::launch(spec);
        }
    }
    crate::bwrap::launch(spec)
}
