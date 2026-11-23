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
use crate::nixenv::{self, EnvMode};
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
    /// Where the container's userland comes from: the host `/usr`, or a hermetic Nix profile.
    pub env: EnvMode,
    /// For `EnvMode::Nix`: the realised profile store path (its `/bin` becomes the whole PATH).
    pub profile: Option<PathBuf>,
    pub extra: Vec<Bind>,
    pub inner: Vec<OsString>,
}

/// The shared read-only system layer plus the project's rw root and virtual filesystems,
/// as backend-neutral ops. Applied in order.
pub(crate) fn base_ops(spec: &Spec) -> Vec<Op> {
    let mut v: Vec<Op> = Vec::new();
    // The shared /nix/store underpins both the host toolchain (if any) and the whole Nix
    // userland, so it is bound read-only in every mode.
    if Path::new("/nix").exists() {
        v.push(Op::Bind {
            src: "/nix".into(),
            dst: "/nix".into(),
            ro: true,
        });
    }
    // The system layer: either inherit the host's /usr + /etc, or build a hermetic root whose
    // only userland is the project's Nix profile.
    match spec.env {
        EnvMode::Host => host_system_layer(&mut v),
        EnvMode::Nix => nix_system_layer(&mut v, spec),
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
        // In host mode resolv.conf may be a symlink into /run/…; bind onto its real path. In
        // hermetic mode /etc is our synthesized dir, so bind onto the literal path.
        let target = match spec.env {
            EnvMode::Host => std::fs::canonicalize("/etc/resolv.conf")
                .unwrap_or_else(|_| PathBuf::from("/etc/resolv.conf")),
            EnvMode::Nix => PathBuf::from("/etc/resolv.conf"),
        };
        v.push(Op::Bind {
            src: resolv,
            dst: target,
            ro: true,
        });
    }
    v
}

/// Inherit the host's read-only system layer: /usr, /etc, and the merged-usr symlinks.
fn host_system_layer(v: &mut Vec<Op>) {
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
}

/// Build a hermetic root: no host /usr. The only userland is `<profile>/bin` (reached through
/// the /nix/store bind), plus a synthesized /etc and the two paths the world hardcodes —
/// `/bin/sh` and `/usr/bin/env` — symlinked into the profile.
fn nix_system_layer(v: &mut Vec<Op>, spec: &Spec) {
    let profile = spec
        .profile
        .clone()
        .expect("EnvMode::Nix requires a realised profile");
    // Synthesized /etc (passwd/group/resolv.conf).
    v.push(Op::Bind {
        src: nixenv::etc_dir(&spec.project),
        dst: "/etc".into(),
        ro: true,
    });
    // The two hardcoded entry points, pointed at the profile.
    v.push(Op::Symlink {
        target: profile.join("bin/sh"),
        dst: "/bin/sh".into(),
    });
    v.push(Op::Symlink {
        target: profile.join("bin/env"),
        dst: "/usr/bin/env".into(),
    });
}

/// Environment variables every container gets. In hermetic Nix mode this also establishes the
/// *entire* environment: PATH is just the profile's bin, and TLS/HOME are pointed at the
/// profile (the host env is cleared first — see `clears_env`).
pub(crate) fn env_pairs(spec: &Spec) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("TUP_PROJECT", spec.project.clone()),
        ("TUP_NET", spec.net.label().to_string()),
    ];
    if spec.env == EnvMode::Nix {
        if let Some(p) = &spec.profile {
            v.push(("PATH", format!("{}/bin", p.display())));
            let ca = nixenv::ca_bundle(p).display().to_string();
            v.push(("SSL_CERT_FILE", ca.clone()));
            v.push(("NIX_SSL_CERT_FILE", ca));
        }
        v.push(("HOME", "/work".to_string()));
        // Preserve the terminal type across the env clear so interactive shells behave.
        v.push(("TERM", std::env::var("TERM").unwrap_or_else(|_| "xterm".into())));
    }
    v
}

/// Whether the backend should clear the inherited host environment before applying
/// `env_pairs`. True for hermetic Nix mode, so nothing from the host leaks in via env.
pub(crate) fn clears_env(spec: &Spec) -> bool {
    spec.env == EnvMode::Nix
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
