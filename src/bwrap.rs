//! The default backend: render a `Spec` into a bubblewrap invocation.
//!
//! bwrap does the user/mount/pid/ipc/uts namespaces and the bind construction; pasta
//! (via `wrap_pasta`) supplies the netns for `Net::Pasta`.

use crate::net::Net;
use crate::sandbox::{base_ops, clears_env, env_pairs, wrap_pasta, Op, Spec};
use std::ffi::OsString;
use std::process::Command;

fn push3(a: &mut Vec<OsString>, flag: &str, x: impl Into<OsString>, y: impl Into<OsString>) {
    a.push(flag.into());
    a.push(x.into());
    a.push(y.into());
}

/// bwrap's argument vector, up to and including the trailing `--` and the command.
fn render(spec: &Spec) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    for s in [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
    ] {
        a.push(s.into());
    }
    if spec.net == Net::None {
        a.push("--unshare-net".into()); // isolated, no egress
    }
    for op in base_ops(spec) {
        match op {
            Op::Bind { src, dst, ro } => {
                let flag = if ro { "--ro-bind" } else { "--bind" };
                push3(&mut a, flag, src, dst);
            }
            Op::Symlink { target, dst } => push3(&mut a, "--symlink", target, dst),
            Op::Proc(d) => {
                a.push("--proc".into());
                a.push(d.into());
            }
            Op::Dev(d) => {
                a.push("--dev".into());
                a.push(d.into());
            }
            Op::Tmpfs(d) => {
                a.push("--tmpfs".into());
                a.push(d.into());
            }
        }
    }
    a.push("--chdir".into());
    a.push(spec.chdir.clone().into());
    // Hermetic env: wipe the inherited host environment, then set only our own.
    if clears_env(spec) {
        a.push("--clearenv".into());
    }
    for (k, v) in env_pairs(spec) {
        push3(&mut a, "--setenv", k, v);
    }
    // Opt-in hardening: drop all capabilities (empty cap set even on a namespace escape)
    // and start a new session (no TIOCSTI injection into the parent terminal).
    if spec.harden {
        for s in ["--cap-drop", "ALL", "--new-session"] {
            a.push(s.into());
        }
    }
    if spec.as_pid1 {
        a.push("--as-pid-1".into());
    }
    a.push("--".into());
    a.extend(spec.inner.iter().cloned());
    a
}

pub(crate) fn launch(spec: Spec) -> Command {
    let args = render(&spec);
    wrap_pasta(spec.net, "bwrap", &args, &spec.ports)
}
