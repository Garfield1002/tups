//! Host-side credential-agent broker (for `outputs.tup.agents`).
//!
//! The security story: a container must be able to *use* a credential without being able to
//! *read* it. So tup never binds a private key into the container — it runs a dedicated
//! `ssh-agent` on the host holding only a tup-managed deploy key, and forwards that agent's
//! **socket** into the container. git/ssh inside the box asks the agent (over the socket) to
//! sign auth challenges; the key itself never crosses the boundary, and the agent dies with
//! the session. The key is scoped (a per-repo deploy key), so the forwarded capability is
//! "push this one repo" and nothing more.
//!
//! The key lives at `~/.tup/keys/<name>` — outside every path tup binds into the container.

use crate::project::project_dir;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

/// A running dedicated ssh-agent: its socket (to forward) and pid (to reap on teardown).
pub(crate) struct Agent {
    pub sock: PathBuf,
    pid: i32,
}

impl Agent {
    /// Stop the agent and remove its socket. Best-effort — called once the container exits.
    pub(crate) fn stop(&self) {
        if self.pid > 0 {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
        }
        let _ = fs::remove_file(&self.sock);
    }
}

fn keys_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".tup").join("keys")
}

/// Ensure the tup-managed deploy key `<name>` exists at `~/.tup/keys/<name>`, generating an
/// ed25519 key (no passphrase) on first use and printing its public half plus the one-time
/// "add this as a deploy key" instruction. Returns the private key's path (host-only).
fn ensure_key(name: &str) -> io::Result<PathBuf> {
    let dir = keys_dir();
    fs::create_dir_all(&dir)?;
    let key = dir.join(name);
    if key.exists() {
        return Ok(key);
    }
    eprintln!("[tup] generating deploy key '{name}' at {}", key.display());
    let st = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C"])
        .arg(format!("tup:{name}"))
        .arg("-f")
        .arg(&key)
        .status()
        .map_err(|e| io::Error::new(e.kind(), format!("ssh-keygen: {e} (is openssh installed?)")))?;
    if !st.success() {
        return Err(io::Error::other(format!("ssh-keygen failed ({st})")));
    }
    if let Ok(pubkey) = fs::read_to_string(key.with_extension("pub")) {
        eprintln!(
            "\n[tup] ONE-TIME SETUP — add this as a deploy key (with write access) on the repo:\n\n{}\n  GitHub: repo → Settings → Deploy keys → Add deploy key (check \"Allow write access\").\n  Until you do, git auth from inside the container will be denied.\n",
            pubkey.trim()
        );
    }
    Ok(key)
}

/// Start a dedicated ssh-agent holding only `keypath`, bound to a per-project socket.
fn start(keypath: &PathBuf, project: &str) -> io::Result<Agent> {
    let sock = project_dir(project).join("agent.sock");
    let _ = fs::remove_file(&sock); // clear any stale socket so `ssh-agent -a` can bind
    if let Some(p) = sock.parent() {
        fs::create_dir_all(p)?;
    }
    let out = Command::new("ssh-agent")
        .arg("-a")
        .arg(&sock)
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("ssh-agent: {e} (is openssh installed?)")))?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "ssh-agent failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // ssh-agent prints `SSH_AGENT_PID=<n>; export SSH_AGENT_PID;` — parse it so we can reap it.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pid = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("SSH_AGENT_PID="))
        .and_then(|rest| rest.split(';').next())
        .and_then(|n| n.trim().parse::<i32>().ok())
        .unwrap_or(-1);
    let agent = Agent { sock, pid };

    // Load the (only) key into this agent.
    let st = Command::new("ssh-add")
        .arg("-t")
        .arg("1800") // bound the signing window to 30 min; the agent is also killed on teardown
        .arg(keypath)
        .env("SSH_AUTH_SOCK", &agent.sock)
        .status()
        .map_err(|e| io::Error::new(e.kind(), format!("ssh-add: {e}")));
    match st {
        Ok(s) if s.success() => Ok(agent),
        Ok(s) => {
            agent.stop();
            Err(io::Error::other(format!("ssh-add failed ({s})")))
        }
        Err(e) => {
            agent.stop();
            Err(e)
        }
    }
}

/// Ensure the deploy key exists and start a dedicated agent holding only it; the returned
/// `Agent.sock` is what the caller binds into the container at `sandbox::AGENT_SOCK`.
pub(crate) fn setup(name: &str, project: &str) -> io::Result<Agent> {
    let key = ensure_key(name)?;
    start(&key, project)
}
