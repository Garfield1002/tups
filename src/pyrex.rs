//! Pyrex — a small, cooperative, in-house bubblewrap backend.
//!
//! Compiled in only under `--features pyrex`, and opt-in at runtime via `TUP_SANDBOX=pyrex`.
//! It realises a `Spec` with raw namespace syscalls instead of shelling out to bwrap. This
//! is deliberately *not* a security sandbox: it skips the symlink-attack-safe mount
//! construction, seccomp, and full capability scrubbing that bwrap does, because tup's
//! model is a cooperative-but-fallible agent (see DESIGN.md "Goals & non-goals").
//!
//! Why it exists: owning the fork gives us the sandbox PID 1's host-visible pid for free
//! (we write it to `~/.tup/<project>/init.pid`), which removes the fragile /proc cmdline
//! scan the bwrap backend forces, and lets us own the whole death chain. `launch` re-execs
//! the tup binary as `tup __sandbox <spec>`, wrapped in pasta exactly like the bwrap path.
//!
//! Process shape:
//!   tup up ──> [pasta] ──> tup __sandbox (monitor, host pid ns; writes pidfile, waits)
//!                               └─fork─> child = PID 1 in new pid ns (mounts, pivot, exec)

use crate::net::{net_from_str, Net};
use crate::project::project_dir;
use crate::nixenv::{env_mode_from_str, EnvMode};
use crate::sandbox::{base_ops, env_pairs, wrap_pasta, Bind, Op, Spec};
use std::ffi::{CString, OsString};
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

/// Runtime switch: the backend is compiled in but still off unless asked for.
/// Which backend to launch.
pub(crate) enum Choice {
    Pyrex,
    Bwrap,
}

/// Decide the backend from `TUP_SANDBOX` and host capability. An explicit value wins (and an
/// unrecognized one warns rather than silently picking a backend the user didn't name);
/// otherwise pyrex is the default, falling back to bwrap where this host can't create a user
/// namespace.
pub(crate) fn choose() -> Choice {
    match std::env::var("TUP_SANDBOX").ok().as_deref() {
        Some("bwrap") => Choice::Bwrap,
        // Explicit opt-in: honor it even if the userns probe would say no (the user may know
        // their setup better than the probe, e.g. a capability we don't test for).
        Some("pyrex") => Choice::Pyrex,
        Some(other) => {
            eprintln!(
                "[tup] warning: unknown TUP_SANDBOX '{other}' (want 'pyrex' or 'bwrap'); using the default"
            );
            default_choice()
        }
        None => default_choice(),
    }
}

/// The default when `TUP_SANDBOX` is unset: pyrex, unless this host can't make an unprivileged
/// user namespace — then fall back to bwrap (which a setuid install can still satisfy) rather
/// than hard-fail a path the previous bwrap default handled.
fn default_choice() -> Choice {
    if userns_available() {
        Choice::Pyrex
    } else {
        eprintln!("[tup] note: unprivileged user namespaces unavailable here; using the bwrap backend");
        Choice::Bwrap
    }
}

/// Probe whether an unprivileged user namespace can be created, in a throwaway forked child so
/// the current process is left untouched. Called only on the default path (not for an explicit
/// `TUP_SANDBOX`), and before tup spawns any threads, so the `fork` is safe.
fn userns_available() -> bool {
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            let r = libc::unshare(libc::CLONE_NEWUSER);
            libc::_exit(if r == 0 { 0 } else { 1 });
        }
        if pid < 0 {
            return false;
        }
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }
}

/// Host side: build the `pasta? -- tup __sandbox <spec> -- <cmd>` launch command.
pub(crate) fn launch(spec: Spec) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let launcher = exe.to_string_lossy().into_owned();

    let mut a: Vec<OsString> = vec![
        "__sandbox".into(),
        "--root".into(),
        spec.root.clone().into_os_string(),
        "--project".into(),
        OsString::from(&spec.project),
        "--net".into(),
        OsString::from(spec.net.label()),
        "--chdir".into(),
        OsString::from(&spec.chdir),
        "--env".into(),
        OsString::from(spec.env.label()),
    ];
    if let Some(p) = &spec.profile {
        a.push("--nix-profile".into());
        a.push(p.clone().into_os_string());
    }
    if spec.harden {
        a.push("--harden".into());
    }
    if spec.as_pid1 {
        a.push("--as-pid1".into());
    }
    if spec.forward_agent {
        a.push("--ssh-agent".into());
    }
    for b in &spec.extra {
        a.push(if b.ro { "--ro-bind" } else { "--bind" }.into());
        a.push(b.src.clone().into_os_string());
        a.push(b.dst.clone().into_os_string());
    }
    a.push("--".into());
    a.extend(spec.inner.iter().cloned());

    wrap_pasta(spec.net, &launcher, &a)
}

// ---------------------------------------------------------------------------
// tup __sandbox — the in-container launcher (runs inside pasta's ns for Net::Pasta)
// ---------------------------------------------------------------------------

pub(crate) fn run_sandbox(args: &[OsString]) -> ! {
    let spec = parse(args);
    // base_ops has one host-side side effect (ensure_resolv); run it before we unshare.
    let ops = base_ops(&spec);
    let outer_uid = unsafe { libc::geteuid() };
    let outer_gid = unsafe { libc::getegid() };

    // 1. New user namespace, then map inner-root -> the uid/gid we currently hold.
    unshare_or_die(libc::CLONE_NEWUSER, "user");
    write_or_die("/proc/self/uid_map", &format!("0 {outer_uid} 1\n"));
    let _ = fs::write("/proc/self/setgroups", "deny");
    write_or_die("/proc/self/gid_map", &format!("0 {outer_gid} 1\n"));

    // 2. New pid namespace in the PARENT: unshare(NEWPID) doesn't move us, but the next
    //    fork becomes PID 1 of the new ns — and the parent keeps the host's pid view, so
    //    fork() hands us that PID 1's host-visible pid directly.
    unshare_or_die(libc::CLONE_NEWPID, "pid");

    // Make the new-root dir here, in the parent's (host) mount namespace, with mkdtemp — a
    // unique, unpredictable name created atomically (no symlink-follow in world-writable /tmp,
    // and not the always-"1" pid of the child). The monitor removes it after the child exits.
    let newroot = match make_newroot() {
        Ok(p) => p,
        Err(e) => die(&format!("new root: {e}")),
    };

    let child = unsafe { libc::fork() };
    if child < 0 {
        die(&format!("fork: {}", io::Error::last_os_error()));
    }
    if child > 0 {
        monitor(&spec, child, &newroot); // never returns
    }

    // ---- child: PID 1 in the new pid namespace ----
    // Die if the monitor dies (ungraceful backstop; the graceful path is a SIGTERM to us).
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) };
    // The child gets its own mount ns (so our pivot_root never touches the monitor's view),
    // plus fresh ipc/uts, and its own empty netns only for Net::None.
    let mut nsflags = libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
    if spec.net == Net::None {
        nsflags |= libc::CLONE_NEWNET;
    }
    unshare_or_die(nsflags, "mount/ipc/uts");

    if let Err(e) = setup_mounts(&ops, &newroot) {
        die(&format!("mount setup: {e}"));
    }
    // Hermetic env: wipe the inherited host environment, then set only our own.
    if crate::sandbox::clears_env(&spec) {
        for (k, _) in std::env::vars() {
            std::env::remove_var(k);
        }
    }
    for (k, v) in env_pairs(&spec) {
        std::env::set_var(k, v);
    }
    if let Err(e) = std::env::set_current_dir(&spec.chdir) {
        die(&format!("chdir {}: {e}", spec.chdir));
    }
    if spec.harden {
        harden_now();
    }
    exec_inner(&spec.inner);
}

// ---------------------------------------------------------------------------
// the monitor (parent): record the pid, forward signals, reap
// ---------------------------------------------------------------------------

static MON_CHILD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn mon_forward(_sig: libc::c_int) {
    let c = MON_CHILD.load(Ordering::SeqCst);
    if c > 0 {
        unsafe { libc::kill(c, libc::SIGTERM) };
    }
}

fn monitor(spec: &Spec, child: i32, newroot: &Path) -> ! {
    // Die with our own parent (tup up, or pasta) so we never linger.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) };

    // The win over the bwrap backend: we know the sandbox PID 1's host pid. Record it so
    // `tup move`/`preflight`/`up` find it exactly, with no /proc cmdline scan.
    let pidfile = project_dir(&spec.project).join("init.pid");
    let _ = fs::write(&pidfile, format!("{child}\n"));

    MON_CHILD.store(child, Ordering::SeqCst);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        let h: extern "C" fn(libc::c_int) = mon_forward;
        sa.sa_sigaction = h as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGTERM, &sa, ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, ptr::null_mut());
    }

    let mut status = 0i32;
    loop {
        let r = unsafe { libc::waitpid(child, &mut status, 0) };
        if r == child {
            break;
        }
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
    }
    let _ = fs::remove_file(&pidfile);
    // The child mounted its tmpfs over newroot inside its OWN mount ns, so in the host ns this
    // is still just the empty dir we mkdtemp'd — remove it so /tmp isn't littered per launch.
    let _ = fs::remove_dir(newroot);
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    };
    exit(code);
}

// ---------------------------------------------------------------------------
// mount construction + pivot_root
// ---------------------------------------------------------------------------

fn setup_mounts(ops: &[Op], newroot: &Path) -> io::Result<()> {
    make_private()?;
    tmpfs(newroot)?; // overlay a fresh tmpfs on the (parent-created) new-root dir

    for op in ops {
        match op {
            Op::Bind { src, dst, ro } => {
                let target = under(newroot, dst);
                prepare_target(&target, src)?;
                bind(src, &target, *ro).map_err(|e| annotate(e, dst))?;
            }
            Op::Symlink { target, dst } => {
                let link = under(newroot, dst);
                if let Some(p) = link.parent() {
                    fs::create_dir_all(p)?;
                }
                // Checked: a missing symlink (e.g. /bin/sh in hermetic mode) would otherwise
                // surface later as an opaque "exec failed" rather than at the real cause.
                symlink(target, &link).map_err(|e| annotate(e, dst))?;
            }
            Op::Proc(d) => {
                let t = under(newroot, d);
                fs::create_dir_all(&t)?;
                procfs(&t).map_err(|e| annotate(e, d))?;
            }
            Op::Dev(d) => {
                let t = under(newroot, d);
                fs::create_dir_all(&t)?;
                setup_dev(&t)?;
            }
            Op::Tmpfs(d) => {
                let t = under(newroot, d);
                fs::create_dir_all(&t)?;
                tmpfs(&t)?;
            }
        }
    }
    pivot(newroot)
}

/// Create a fresh, uniquely-named new-root dir with `mkdtemp`: atomic, mode 0700, and it never
/// follows a symlink planted at a predictable path (the old `/tmp/.tup-root.<pid>` was always
/// `.1`, since the child is PID 1 in its new pid ns). Made in the host mount ns so the monitor
/// can remove the leftover dir on teardown.
fn make_newroot() -> io::Result<PathBuf> {
    let mut tmpl = b"/tmp/.tup-root.XXXXXX\0".to_vec();
    let p = unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    let end = tmpl.iter().position(|&b| b == 0).unwrap_or(tmpl.len());
    Ok(PathBuf::from(OsString::from_vec(tmpl[..end].to_vec())))
}

/// Create the mountpoint for a bind: a dir if the source is a dir, else an empty file.
fn prepare_target(target: &Path, src: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(target)?;
    } else {
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        File::create(target)?; // checked: a missing mountpoint -> opaque bind failure otherwise
    }
    Ok(())
}

/// /dev: bind the host /dev wholesale, read-WRITE. A fresh tmpfs + bound nodes would inherit
/// the `nodev` flag the kernel forces on new mounts in an unprivileged userns, which makes
/// char devices (even /dev/null) unusable — so we reuse the host's dev-capable mount. It is
/// deliberately rw: the submounts /dev/shm (POSIX shared memory) and /dev/pts (ptys) must be
/// writable, and a ro bind would (with the recursive-ro fix in `bind`) break both. This is a
/// known, accepted divergence from bwrap's minimal `--dev`, consistent with the cooperative
/// model (host device-node permissions still apply; we don't try to hide host devices).
fn setup_dev(dev: &Path) -> io::Result<()> {
    bind(Path::new("/dev"), dev, false)
}

fn pivot(newroot: &Path) -> io::Result<()> {
    fs::create_dir_all(newroot.join("oldroot"))?;
    std::env::set_current_dir(newroot)?;
    let dot = cstr(".");
    let old = cstr("oldroot");
    let r = unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), old.as_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    std::env::set_current_dir("/")?;
    let oldabs = cstr("/oldroot");
    // Fail CLOSED: if the detach fails we must not launch, or the entire host filesystem stays
    // mounted read-write at /oldroot inside the container (reported as success).
    if unsafe { libc::umount2(oldabs.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(io::Error::other(format!(
            "detach old root: {}",
            io::Error::last_os_error()
        )));
    }
    let _ = fs::remove_dir("/oldroot"); // best-effort; an empty dir left inside is harmless
    Ok(())
}

// ---------------------------------------------------------------------------
// hardening + exec
// ---------------------------------------------------------------------------

/// Weaker than bwrap's `--cap-drop ALL` (which scrubs the effective/inheritable sets via
/// capset): here we set no-new-privs, start a new session (no TIOCSTI injection), and drop
/// the capability bounding set. Enough for "lower the escape stakes" in the cooperative
/// model; a full capset scrub is a follow-up.
fn harden_now() {
    unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        libc::setsid();
        let mut cap = 0;
        while cap <= 40 {
            libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            cap += 1;
        }
    }
}

fn exec_inner(inner: &[OsString]) -> ! {
    if inner.is_empty() {
        die("__sandbox: empty command");
    }
    let args: Vec<CString> = inner
        .iter()
        .map(|a| CString::new(a.as_bytes()).expect("nul byte in argv"))
        .collect();
    let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(ptr::null());
    unsafe { libc::execvp(args[0].as_ptr(), ptrs.as_ptr()) };
    die(&format!(
        "exec {:?}: {}",
        inner.first(),
        io::Error::last_os_error()
    ));
}

// ---------------------------------------------------------------------------
// low-level helpers
// ---------------------------------------------------------------------------

fn cstr(s: &str) -> CString {
    CString::new(s).expect("nul byte in str")
}
fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("nul byte in path")
}

fn mount_call(
    src: &CString,
    target: &CString,
    fstype: &CString,
    flags: libc::c_ulong,
) -> io::Result<()> {
    let r = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            ptr::null(),
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn bind(src: &Path, target: &Path, ro: bool) -> io::Result<()> {
    let s = cpath(src);
    let t = cpath(target);
    let none = cstr("none");
    mount_call(&s, &t, &none, libc::MS_BIND | libc::MS_REC)
        .map_err(|e| io::Error::new(e.kind(), format!("bind: {e}")))?;
    if ro {
        // Read-only is a second step: a plain bind can't carry MS_RDONLY. The kernel can't
        // recurse a remount, and the recursive bind above replicated the source's submounts
        // under `target`, so we remount the top AND each submount ro — otherwise a submount
        // (e.g. a separate /usr/local) stays writable and pyrex silently diverges from bwrap's
        // genuinely-recursive --ro-bind.
        remount_ro(target)?;
        for sub in submounts_under(target) {
            remount_ro(&sub)?;
        }
    }
    Ok(())
}

/// Remount one already-bound mountpoint read-only. In an unprivileged userns the remount must
/// preserve the mount's already-set nosuid/nodev/noexec/atime flags — dropping a locked flag
/// is EPERM — so re-apply them from statvfs.
fn remount_ro(target: &Path) -> io::Result<()> {
    let t = cpath(target);
    let none = cstr("none");
    let keep = locked_flags(target);
    mount_call(
        &none,
        &t,
        &none,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | keep,
    )
    .map_err(|e| io::Error::new(e.kind(), format!("remount-ro {}: {e}", target.display())))
}

/// Mountpoints strictly under `target`, from `/proc/self/mountinfo` (field 5 is the mount
/// point). Used to make a recursive-ro bind actually recursive. (System paths here are ASCII,
/// so the octal escaping mountinfo uses for exotic bytes isn't decoded.)
fn submounts_under(target: &Path) -> Vec<PathBuf> {
    let data = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mut out = Vec::new();
    for line in data.lines() {
        // fields: mount-id parent-id major:minor root MOUNTPOINT ...
        if let Some(mp) = line.split(' ').nth(4) {
            let mpp = PathBuf::from(mp);
            if mpp != *target && mpp.starts_with(target) {
                out.push(mpp);
            }
        }
    }
    out
}

/// The per-mount flags currently set on `target`, mapped MS_* so a bind-remount can carry
/// them back (the kernel refuses a remount that would drop a locked flag in a userns).
fn locked_flags(target: &Path) -> libc::c_ulong {
    let t = cpath(target);
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(t.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    let f = s.f_flag;
    let mut m: libc::c_ulong = 0;
    for (st, ms) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (libc::ST_RELATIME, libc::MS_RELATIME),
    ] {
        if f & st != 0 {
            m |= ms;
        }
    }
    m
}

fn tmpfs(target: &Path) -> io::Result<()> {
    let t = cpath(target);
    let f = cstr("tmpfs");
    mount_call(&f, &t, &f, 0)
}

fn procfs(target: &Path) -> io::Result<()> {
    let t = cpath(target);
    let f = cstr("proc");
    mount_call(&f, &t, &f, 0)
}

fn make_private() -> io::Result<()> {
    let none = cstr("none");
    let slash = cstr("/");
    mount_call(&none, &slash, &none, libc::MS_REC | libc::MS_PRIVATE)
}

fn under(root: &Path, dst: &Path) -> PathBuf {
    root.join(dst.strip_prefix("/").unwrap_or(dst))
}

fn annotate(e: io::Error, dst: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", dst.display()))
}

fn unshare_or_die(flags: libc::c_int, what: &str) {
    if unsafe { libc::unshare(flags) } != 0 {
        die(&format!(
            "unshare {what}: {} (unprivileged user namespaces available?)",
            io::Error::last_os_error()
        ));
    }
}

fn write_or_die(path: &str, content: &str) {
    if let Err(e) = fs::write(path, content) {
        die(&format!("write {path}: {e}"));
    }
}

fn die(msg: &str) -> ! {
    eprintln!("[tup __sandbox] {msg}");
    exit(1)
}

// ---------------------------------------------------------------------------
// argv (de)serialization — mirrors `launch` above
// ---------------------------------------------------------------------------

fn parse(args: &[OsString]) -> Spec {
    let mut root = PathBuf::new();
    let mut project = String::new();
    let mut net = Net::None;
    let mut chdir = "/work".to_string();
    let mut harden = false;
    let mut as_pid1 = false;
    let mut forward_agent = false;
    let mut env = EnvMode::Host;
    let mut profile: Option<PathBuf> = None;
    let mut extra: Vec<Bind> = Vec::new();
    let mut inner: Vec<OsString> = Vec::new();
    let mut i = 0;
    // Flag NAMES and the ascii-only values (project/net/chdir/env) match via to_str(); PATHS
    // (--root, --nix-profile, binds, and the inner command after --) keep full OsString
    // fidelity, so a non-UTF-8 path survives the __sandbox re-exec instead of being mangled.
    while i < args.len() {
        match args[i].to_str() {
            Some("--root") => {
                root = PathBuf::from(req(args, i + 1, "--root"));
                i += 2;
            }
            Some("--project") => {
                project = req(args, i + 1, "--project").to_string_lossy().into_owned();
                i += 2;
            }
            Some("--net") => {
                net = net_from_str(args.get(i + 1).and_then(|s| s.to_str()));
                i += 2;
            }
            Some("--chdir") => {
                chdir = req(args, i + 1, "--chdir").to_string_lossy().into_owned();
                i += 2;
            }
            Some("--env") => {
                env = env_mode_from_str(args.get(i + 1).and_then(|s| s.to_str()));
                i += 2;
            }
            Some("--nix-profile") => {
                profile = Some(PathBuf::from(req(args, i + 1, "--nix-profile")));
                i += 2;
            }
            Some("--harden") => {
                harden = true;
                i += 1;
            }
            Some("--as-pid1") => {
                as_pid1 = true;
                i += 1;
            }
            Some("--ssh-agent") => {
                forward_agent = true;
                i += 1;
            }
            Some("--bind") | Some("--ro-bind") => {
                let ro = args[i].to_str() == Some("--ro-bind");
                let s = req(args, i + 1, "bind src");
                let d = req(args, i + 2, "bind dst");
                extra.push(Bind {
                    src: PathBuf::from(s),
                    dst: PathBuf::from(d),
                    ro,
                });
                i += 3;
            }
            Some("--") => {
                inner = args[i + 1..].to_vec();
                break;
            }
            _ => die(&format!(
                "__sandbox: unexpected arg '{}'",
                args[i].to_string_lossy()
            )),
        }
    }
    Spec {
        root,
        project,
        net,
        chdir,
        harden,
        as_pid1,
        env,
        profile,
        forward_agent,
        extra,
        inner,
    }
}

fn req<'a>(args: &'a [OsString], i: usize, what: &str) -> &'a OsString {
    args.get(i)
        .unwrap_or_else(|| die(&format!("__sandbox: missing value for {what}")))
}
