# tups — ultra-micro containers for coding agents

**tups** (command: `tup`) puts a coding agent's project in its own filesystem, network, and
process namespaces, then lets you **move the whole live session to another server** between
turns. The goal is two things:

- **Contain the blast radius.** An agent's mistake stays inside its project — its files, its
  ports, its process tree — not your whole machine.
- **Relocate a session.** Pause an agent on one box, ship it to another, resume it there,
  with the same internal addresses (`127.0.0.1:3000` is the dev server on every server).

It's rootless (user namespaces, no daemon, no `sudo`), the per-project overhead is tiny
(binaries live once in a shared `/nix/store`), and a container starts in ~10 ms.

> [!IMPORTANT]
> **`tup` is not a security boundary.** It assumes a *cooperative-but-fallible* agent. The
> isolation defends against **accidents and mistakes**, not a malicious escape, and egress to
> the internet is wide open by design (the agent needs the model API and package mirrors).
> Do not use it to run untrusted code or as a multi-tenant sandbox. See
> [Security posture](DESIGN.md#security-posture--hardening).
>
> It is also **prototype-grade**: single author, happy paths verified by hand, some paths
> (`--net pasta` nesting, `ssh` move) untested in this environment. Read [`DESIGN.md`](DESIGN.md)
> before relying on it.

## Requirements

Linux with unprivileged user namespaces enabled. On the `PATH`:

- [`passt`/`pasta`](https://passt.top/) — for `--net pasta` (NAT egress from a private netns)
- `util-linux` (`nsenter`) — for `tup attach`
- [`bubblewrap`](https://github.com/containers/bubblewrap) (`bwrap`) — only for the bwrap
  backend (`TUP_SANDBOX=bwrap` or a `--no-default-features` build); the default backend
  (pyrex) needs no external helper
- [`nix`](https://nixos.org/) — only for `--env nix` (the hermetic flake environment)
- `rsync` + `ssh` — only for `tup move`

## Build

```sh
cargo build --release                      # default: includes the pyrex backend
cargo build --release --no-default-features # bwrap-only binary (no raw-namespace backend)
```

The binary is `target/release/tup`. Put it on your `PATH`.

## Quickstart

```sh
# Run a one-shot command in project "web" (private netns with NAT egress, host /usr).
tup run web -- cargo test

# Launch a long-lived, supervised container. Declare services first:
mkdir -p ~/.tup/web
cat > ~/.tup/web/services <<'EOF'
start dev   npm run dev
stop  dev   npm run stop      # optional clean-quiesce hook
EOF
tup up web                     # becomes PID 1, starts services, reaps the tree

# From another terminal:
tup ps                         # list projects and their state
tup attach web                 # join the running container with a shell
tup stop web                   # clean shutdown: runs stop hooks, then a bounded sweep

# Move the session to another server (ships the working tree + persist volumes + env):
tup preflight web              # report what the move will cost / lose
tup move web user@host
```

## Commands

| Command | What it does |
|---|---|
| `tup run [--net M] [--env E] [--harden] <project> [-- <cmd>]` | Run one command in a fresh namespace. |
| `tup up [--net M] [--env E] [--harden] <project>` | Launch a PID-1 supervised container that starts declared services. |
| `tup ps` | List projects: state, PID, net mode, env, service count. |
| `tup stop <project>` | Cleanly stop a running project (quiesce). Idempotent. |
| `tup attach <project> [-- <cmd>]` | Join a running container (a shell by default). |
| `tup rm [--force] <project>` | Delete a project's state. Dry-run unless `--force`; refuses while running. |
| `tup env <project> [list\|add <p>\|rm <p>]` | Inspect/edit the Nix env's prologue selection. |
| `tup move <project> <dest>` | Migrate a session to `dest` (an `ssh` target). |
| `tup preflight <project>` | Report the cost/loss of a move without doing it. |
| `tup help` / `tup version` | Usage / version. |

### Networking (`--net`)

| Mode | Behavior |
|---|---|
| `pasta` *(default)* | Dedicated netns, own loopback, NAT egress to the internet via pasta. |
| `host` | Share the host network namespace. |
| `none` | Isolated netns, loopback only, no egress. |

The chosen mode is persisted per project, so a `tup move` restores it (a `--net none` session
is never silently re-opened to egress).

### Environments (`--env`)

- `host` *(default)* — inherit the host's `/usr` and `/etc`.
- `nix` — **hermetic**: the project's `flake.nix` *is* the environment. The host `/usr` is
  dropped, `PATH` is just the flake profile's `bin`, and the host env is wiped. The closure
  already lives in the shared read-only `/nix/store`, so realizing the env copies nothing.

The Nix env is composed from **prologues** — reusable package profiles. `base` is always
active; opt into languages with `tup env <project> add rust` (also `python`, `node`, `go`,
`c`, `lean`). The one thing a flake can't model — network, persist volumes, and host binds —
lives as plain data in the flake's `outputs.tup`, which tup reads and applies:

```nix
tup = {
  net = "pasta";                       # overrides the default net mode
  persist = [ "/var/lib/postgresql" ]; # rw dirs that survive teardown and ride a move
  binds = [ { path = "/etc/hosts"; ro = true; } ];  # host files to link in
};
```

### Hardening (`--harden`)

Drops all capabilities and starts a new session (TTY-injection proof). This lowers the escape
stakes; it does **not** turn `tup` into a security boundary.

## Backends

`tup` renders one backend-neutral container spec through either of two backends:

- **pyrex** *(default)* — an in-house backend using raw namespace syscalls. No external helper
  binary; owning the `fork` gives tup the sandbox PID 1's host pid directly (written to
  `init.pid`). It is **not** a security sandbox (weaker mount/cap handling than bwrap) — which
  is fine here, since tups is a cooperative-model tool, not a boundary.
- **bubblewrap** — shells out to `bwrap` (audited mount handling). Select it with
  `TUP_SANDBOX=bwrap`, or build a bwrap-only binary with `--no-default-features`.

Both render the same ordered mount/op list, so they can't drift.

## Project state layout

Everything for a project lives under `~/.tup/<project>/`:

```
root/        the rw working tree (the agent's files)
services     the service manifest (start/stop directives)
net          the persisted net mode
init.pid     the sandbox PID 1 (pyrex backend)
persist/     backing dirs for declared persist volumes
env/         the Nix flake, prologue selection, and synthesized /etc (--env nix)
```

## Development

```sh
cargo test --release                       # unit tests (pure logic)
cargo clippy --release --all-targets        # lint
cargo clippy --release --all-targets --features pyrex
```

## Learn more

[`DESIGN.md`](DESIGN.md) is the full design: the model, the migration protocol, the backend
seam, the Nix integration, the decision log, and the security posture.
