//! The container spec and the backend seam.
//!
//! A `Spec` describes *what* container to build; a backend decides *how*. The system
//! layer is expressed once, as a backend-neutral list of `Op`s (`base_ops`), so the
//! `bwrap` backend (which renders them to bwrap flags) and the `pyrex` backend (which
//! renders them to raw mount syscalls) can never drift apart.
//!
//! Networking: for `Net::Pasta` the launcher is wrapped in pasta (outer netns + NAT
//! egress + DNS forward); the backend's own process runs *inside* pasta's namespaces.

use crate::net::{ensure_resolv, Net, PortForward, Proto, PASTA_DNS};
use crate::nixenv::{self, EnvMode};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a forwarded credential-agent socket appears inside the container. The caller binds
/// the host agent's socket here (via `extra`) and sets `forward_agent` so `SSH_AUTH_SOCK`
/// points at it; the agent's key never enters the container. (A sibling of `/run/tup`, the
/// supervisor binary — not under it, since that path is a file.)
pub(crate) const AGENT_SOCK: &str = "/run/tup-agent.sock";

/// Where a `gate`'s socket appears inside the container (a sibling of the `/run/tup` binary).
/// The container holds only this socket; the host-side gatekeeper performs the filtered op.
pub(crate) const GATE_SOCK: &str = "/run/tup-gate.sock";

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
    /// Port forwards to install in pasta (ignored for `Net::Host`/`Net::None`).
    pub ports: Vec<PortForward>,
    pub inner: Vec<OsString>,
}

/// Parse a `--map HOST:CONTAINER[:ro]` spec into a `Bind` with distinct host/container paths
/// (rw by default; a trailing `:ro` — or `:rw` — sets access). Paths may rarely contain a
/// colon, so the access suffix is stripped first and the remainder split on the LAST colon;
/// both sides must be absolute.
pub(crate) fn map_from_str(s: &str) -> Result<Bind, String> {
    let s = s.trim();
    let (body, ro) = if let Some(rest) = s.strip_suffix(":ro") {
        (rest, true)
    } else if let Some(rest) = s.strip_suffix(":rw") {
        (rest, false)
    } else {
        (s, false)
    };
    let (host, container) = body
        .rsplit_once(':')
        .ok_or_else(|| format!("--map needs HOST:CONTAINER[:ro] (got {s:?})"))?;
    if host.is_empty() || container.is_empty() {
        return Err(format!("--map needs both HOST and CONTAINER paths (got {s:?})"));
    }
    if !host.starts_with('/') || !container.starts_with('/') {
        return Err(format!("--map paths must be absolute (got {s:?})"));
    }
    Ok(Bind {
        src: PathBuf::from(host),
        dst: PathBuf::from(container),
        ro,
    })
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

/// Wrap a launcher argv in pasta when `Net::Pasta`; otherwise run it directly. `ports` adds
/// inbound host→guest port forwards (pasta's `--tcp-ports`/`--udp-ports`, each `HOST:CONTAINER`
/// where HOST is pasta's local port and CONTAINER the in-namespace port); only used for pasta.
pub(crate) fn wrap_pasta(net: Net, launcher: &str, args: &[OsString], ports: &[PortForward]) -> Command {
    match net {
        Net::Pasta => {
            let mut c = Command::new("pasta");
            c.arg("--config-net")
                .arg("--dns-forward")
                .arg(PASTA_DNS)
                .arg("--quiet");
            // Explicit port forwards. Giving any `--tcp-ports`/`--udp-ports` overrides pasta's
            // default `auto` for that protocol, which is what an explicit `--port` wants. Flag
            // syntax verified against `man pasta`: `-t 18080:3000` forwards the host's 18080 to
            // port 3000 in the target namespace. Can be repeated, so one flag per forward.
            for p in ports {
                let flag = match p.proto {
                    Proto::Tcp => "--tcp-ports",
                    Proto::Udp => "--udp-ports",
                };
                c.arg(flag).arg(format!("{}:{}", p.host, p.container));
            }
            // pasta execs its very first child specially — before its own uid/gid map is in force
            // for that execve — so a launcher binary owned by the (as-yet-unmapped) caller is
            // refused with EACCES, while a root-owned one runs. Interpose root-owned /usr/bin/env:
            // by the time env exec()s the launcher the map is live and the launcher reads as
            // owner-root. env runs on the host, before the sandbox; it's a harmless no-op layer
            // for the already-root-owned bwrap launcher too. The launcher path is absolute, so env
            // never mistakes it for an option.
            c.arg("--").arg("/usr/bin/env").arg(launcher).args(args);
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
    use super::{map_from_str, AGENT_SOCK};
    use std::path::Path;

    #[test]
    fn map_rw_by_default() {
        let b = map_from_str("/host/data:/work/data").unwrap();
        assert_eq!(b.src, Path::new("/host/data"));
        assert_eq!(b.dst, Path::new("/work/data"));
        assert!(!b.ro);
    }

    #[test]
    fn map_ro_suffix() {
        let b = map_from_str("/host/data:/work/data:ro").unwrap();
        assert_eq!(b.src, Path::new("/host/data"));
        assert_eq!(b.dst, Path::new("/work/data"));
        assert!(b.ro);
    }

    #[test]
    fn map_rw_suffix_is_explicit_rw() {
        let b = map_from_str("/a:/b:rw").unwrap();
        assert_eq!(b.dst, Path::new("/b"));
        assert!(!b.ro);
    }

    #[test]
    fn map_splits_on_last_colon() {
        // A host path that itself contains a colon: the HOST/CONTAINER split is the LAST colon,
        // so the colon stays on the host side and the container is the final absolute segment.
        let b = map_from_str("/ho:st:/work/data").unwrap();
        assert_eq!(b.src, Path::new("/ho:st"));
        assert_eq!(b.dst, Path::new("/work/data"));
        assert!(!b.ro);
        // ...and the same with an access suffix still parses.
        let b = map_from_str("/ho:st:/work/data:ro").unwrap();
        assert_eq!(b.src, Path::new("/ho:st"));
        assert_eq!(b.dst, Path::new("/work/data"));
        assert!(b.ro);
    }

    #[test]
    fn map_rejects_malformed() {
        for bad in [
            "",                    // empty
            "/onlyhost",           // no container (no colon)
            "relative:/work/x",    // host not absolute
            "/host/x:relative",    // container not absolute
            ":/work/x",            // empty host
            "/host/x:",            // empty container
            "/host/x::ro",         // empty container with ro suffix
        ] {
            assert!(map_from_str(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn agent_sock_is_a_sibling_of_the_supervisor_binary() {
        // /run/tup is the tup binary (a file); the agent socket must NOT be a child of it
        // (that would need /run/tup to be a directory), but a sibling under /run.
        assert!(!AGENT_SOCK.starts_with("/run/tup/"), "would collide with the /run/tup file");
        assert!(AGENT_SOCK.starts_with("/run/"));
    }
}
