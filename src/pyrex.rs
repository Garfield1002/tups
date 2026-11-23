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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

/// Runtime switch: the backend is compiled in but still off unless asked for.
pub(crate) fn selected() -> bool {
    std::env::var("TUP_SANDBOX").map(|v| v == "pyrex").unwrap_or(false)
}

/// Host side: build the `pasta? -- tup __sandbox <spec> -- <cmd>` launch command.
pub(crate) fn launch(spec: Spec) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let launcher = exe.to_string_lossy().into_owned();

    let mut a: Vec<OsString> = Vec::new();
    a.push("__sandbox".into());
    a.push("--root".into());
    a.push(spec.root.clone().into_os_string());
    a.push("--project".into());
    a.push(OsString::from(&spec.project));
    a.push("--net".into());
    a.push(OsString::from(spec.net.label()));
    a.push("--chdir".into());
    a.push(OsString::from(&spec.chdir));
    a.push("--env".into());
    a.push(OsString::from(spec.env.label()));
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

pub(crate) fn run_sandbox(args: &[String]) -> ! {
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

    let child = unsafe { libc::fork() };
    if child < 0 {
        die(&format!("fork: {}", io::Error::last_os_error()));
    }
    if child > 0 {
        monitor(&spec, child); // never returns
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

    if let Err(e) = setup_mounts(&ops) {
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

fn monitor(spec: &Spec, child: i32) -> ! {
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

fn setup_mounts(ops: &[Op]) -> io::Result<()> {
    make_private()?;
    let newroot = PathBuf::from(format!("/tmp/.tup-root.{}", std::process::id()));
    fs::create_dir_all(&newroot)?;
    tmpfs(&newroot)?;

    for op in ops {
        match op {
            Op::Bind { src, dst, ro } => {
                let target = under(&newroot, dst);
                prepare_target(&target, src)?;
                bind(src, &target, *ro).map_err(|e| annotate(e, dst))?;
            }
            Op::Symlink { target, dst } => {
                let link = under(&newroot, dst);
                if let Some(p) = link.parent() {
                    fs::create_dir_all(p)?;
                }
                let _ = symlink(target, &link);
            }
            Op::Proc(d) => {
                let t = under(&newroot, d);
                fs::create_dir_all(&t)?;
                procfs(&t).map_err(|e| annotate(e, d))?;
            }
            Op::Dev(d) => {
                let t = under(&newroot, d);
                fs::create_dir_all(&t)?;
                setup_dev(&t)?;
            }
            Op::Tmpfs(d) => {
                let t = under(&newroot, d);
                fs::create_dir_all(&t)?;
                tmpfs(&t)?;
            }
        }
    }
    pivot(&newroot)
}

/// Create the mountpoint for a bind: a dir if the source is a dir, else an empty file.
fn prepare_target(target: &Path, src: &Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(target)?;
    } else {
        if let Some(p) = target.parent() {
            fs::create_dir_all(p)?;
        }
        let _ = File::create(target);
    }
    Ok(())
}

/// /dev: bind the host /dev wholesale. A fresh tmpfs + bound nodes would inherit the
/// `nodev` flag the kernel forces on new mounts in an unprivileged userns, which makes
/// char devices (even /dev/null) unusable — so we reuse the host's dev-capable mount.
/// Cooperative model: no attempt to hide host devices.
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
    unsafe { libc::umount2(oldabs.as_ptr(), libc::MNT_DETACH) };
    let _ = fs::remove_dir("/oldroot");
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
        // Read-only is a second step: a plain bind can't carry MS_RDONLY. No MS_REC (the
        // kernel can't recursively remount in one call). Crucially, in an unprivileged
        // userns the remount must preserve the mount's already-set nosuid/nodev/noexec/
        // atime flags — dropping a locked flag is EPERM — so re-apply them from statvfs.
        let keep = locked_flags(target);
        mount_call(
            &none,
            &t,
            &none,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | keep,
        )
        .map_err(|e| io::Error::new(e.kind(), format!("remount-ro: {e}")))?;
    }
    Ok(())
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

fn parse(args: &[String]) -> Spec {
    let mut root = PathBuf::new();
    let mut project = String::new();
    let mut net = Net::None;
    let mut chdir = "/work".to_string();
    let mut harden = false;
    let mut as_pid1 = false;
    let mut env = EnvMode::Host;
    let mut profile: Option<PathBuf> = None;
    let mut extra: Vec<Bind> = Vec::new();
    let mut inner: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                root = PathBuf::from(req(args, i + 1, "--root"));
                i += 2;
            }
            "--project" => {
                project = req(args, i + 1, "--project").clone();
                i += 2;
            }
            "--net" => {
                net = net_from_str(args.get(i + 1).map(String::as_str));
                i += 2;
            }
            "--chdir" => {
                chdir = req(args, i + 1, "--chdir").clone();
                i += 2;
            }
            "--env" => {
                env = env_mode_from_str(args.get(i + 1).map(String::as_str));
                i += 2;
            }
            "--nix-profile" => {
                profile = Some(PathBuf::from(req(args, i + 1, "--nix-profile")));
                i += 2;
            }
            "--harden" => {
                harden = true;
                i += 1;
            }
            "--as-pid1" => {
                as_pid1 = true;
                i += 1;
            }
            "--bind" | "--ro-bind" => {
                let ro = args[i] == "--ro-bind";
                let s = req(args, i + 1, "bind src");
                let d = req(args, i + 2, "bind dst");
                extra.push(Bind {
                    src: PathBuf::from(s),
                    dst: PathBuf::from(d),
                    ro,
                });
                i += 3;
            }
            "--" => {
                inner = args[i + 1..].iter().map(OsString::from).collect();
                break;
            }
            other => die(&format!("__sandbox: unexpected arg '{other}'")),
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
        extra,
        inner,
    }
}

fn req<'a>(args: &'a [String], i: usize, what: &str) -> &'a String {
    args.get(i)
        .unwrap_or_else(|| die(&format!("__sandbox: missing value for {what}")))
}
