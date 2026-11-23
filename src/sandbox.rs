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

/// Where a forwarded credential-agent socket appears inside the container. The caller binds
/// the host agent's socket here (via `extra`) and sets `forward_agent` so `SSH_AUTH_SOCK`
/// points at it; the agent's key never enters the container. (A sibling of `/run/tup`, the
/// supervisor binary — not under it, since that path is a file.)
pub(crate) const AGENT_SOCK: &str = "/run/tup-agent.sock";

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
    /// A credential agent's socket is bound (via `extra`) at `AGENT_SOCK`; set `SSH_AUTH_SOCK`.
    pub forward_agent: bool,
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
    // The one host binary that must run *inside* the hermetic root is tup itself, as the
    // `tup up` PID-1 supervisor (bound at /run/tup). It's a glibc binary, so bind its own
    // loader + libs — the loader at its absolute ELF-interp path, the libraries into /usr/lib
    // (a default search dir, so no ld.so.cache or LD_LIBRARY_PATH is needed, and the agent's
    // profile binaries — which resolve their libc via /nix/store RUNPATHs — never look here).
    if spec.as_pid1 {
        // The kernel opens the loader at the exe's ELF-interp path (often a symlink like
        // /lib64/ld-linux-…), which differs from the resolved real path /proc/self/maps shows.
        let interp = std::env::current_exe().ok().and_then(|e| elf_interp(&e));
        for lib in self_libs() {
            let name = match lib.file_name() {
                Some(n) => n.to_owned(),
                None => continue,
            };
            // Libraries go in /usr/lib (a default loader search dir).
            v.push(Op::Bind {
                src: lib.clone(),
                dst: PathBuf::from("/usr/lib").join(&name),
                ro: true,
            });
            // The loader additionally at the exact interp path the kernel will open.
            if is_loader(&name) {
                if let Some(ip) = &interp {
                    v.push(Op::Bind {
                        src: lib,
                        dst: PathBuf::from(ip),
                        ro: true,
                    });
                }
            }
        }
    }
}

fn is_loader(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with("ld-")
}

/// Read the program interpreter (PT_INTERP) path out of an ELF64 little-endian binary — the
/// path the kernel opens to run it, which may be a symlink distinct from the real loader file.
fn elf_interp(exe: &Path) -> Option<String> {
    let d = std::fs::read(exe).ok()?;
    if d.len() < 64 || &d[0..4] != b"\x7fELF" || d[4] != 2 {
        return None; // not ELF64
    }
    let u64le = |o: usize| -> Option<usize> { Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?) as usize) };
    let u16le = |o: usize| -> Option<usize> { Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?) as usize) };
    let phoff = u64le(32)?;
    let phentsize = u16le(54)?;
    let phnum = u16le(56)?;
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        let p_type = u32::from_le_bytes(d.get(off..off + 4)?.try_into().ok()?);
        if p_type == 3 {
            // PT_INTERP: p_offset @ off+8, p_filesz @ off+32
            let p_offset = u64le(off + 8)?;
            let p_filesz = u64le(off + 32)?;
            let end = (p_offset + p_filesz).min(d.len());
            let bytes = d.get(p_offset..end)?;
            let s = bytes.split(|b| *b == 0).next()?;
            return Some(String::from_utf8_lossy(s).into_owned());
        }
    }
    None
}

/// The shared objects this tup process has mapped (its loader + libc + libgcc), read from
/// `/proc/self/maps`. Used to make the `tup` PID-1 supervisor runnable inside a hermetic root.
/// Store paths are skipped (already visible via the `/nix` bind) and so is the exe itself
/// (bound separately at `/run/tup`).
fn self_libs() -> Vec<PathBuf> {
    let me = std::env::current_exe().ok();
    let mut out: Vec<PathBuf> = Vec::new();
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(m) => m,
        Err(_) => return out,
    };
    for line in maps.lines() {
        let path = match line.find(" /") {
            Some(i) => line[i + 1..].trim(),
            None => continue,
        };
        if !path.starts_with('/') || path.starts_with("/nix/") || path.ends_with("(deleted)") {
            continue;
        }
        let p = PathBuf::from(path);
        if Some(&p) == me.as_ref() || out.contains(&p) {
            continue;
        }
        if p.is_file() {
            out.push(p);
        }
    }
    out
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
    // Point ssh/git at the forwarded agent socket (set in both env modes; in Nix mode the host
    // env was cleared, so this is the only SSH_AUTH_SOCK, and in host mode it overrides the
    // inherited one, which points at a host path that isn't bound into the container).
    if spec.forward_agent {
        v.push(("SSH_AUTH_SOCK", AGENT_SOCK.to_string()));
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

/// Build the launch `Command`, selecting the backend. The `pyrex` backend is the default
/// (compiled in unless `--no-default-features`); `TUP_SANDBOX=bwrap` opts back into the
/// bubblewrap backend, which is also the only option in a `--no-default-features` build.
pub(crate) fn launch(spec: Spec) -> Command {
    #[cfg(feature = "pyrex")]
    {
        // Early-return only for pyrex; the Bwrap choice falls through to the tail below, which
        // is also the sole path in a `--no-default-features` build.
        if let crate::pyrex::Choice::Pyrex = crate::pyrex::choose() {
            return crate::pyrex::launch(spec);
        }
    }
    crate::bwrap::launch(spec)
}

#[cfg(test)]
mod tests {
    use super::AGENT_SOCK;

    #[test]
    fn agent_sock_is_a_sibling_of_the_supervisor_binary() {
        // /run/tup is the tup binary (a file); the agent socket must NOT be a child of it
        // (that would need /run/tup to be a directory), but a sibling under /run.
        assert!(!AGENT_SOCK.starts_with("/run/tup/"), "would collide with the /run/tup file");
        assert!(AGENT_SOCK.starts_with("/run/"));
    }
}
