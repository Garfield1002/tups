# tups

**Ultra-micro containers for coding agents.** `tups` (command: `tup`) gives a coding agent
its own filesystem, network, and process namespaces, and lets you **move the live session to
another machine** between turns.

```sh
tup run web -- cargo test        # one-shot, in a private namespace
tup up  web                      # long-lived, supervised, PID 1
tup move web user@host           # ship the whole session elsewhere, resume it there
```

## What it does

- **Contains the blast radius.** A mistake stays inside the project — its files, its ports,
  its process tree — instead of reaching the rest of your machine.
- **Relocates a session.** Pause an agent on one box, ship it to another, resume it there —
  with the same internal addresses, so `127.0.0.1:3000` is the dev server on *every* server.
- **Lends capabilities, not secrets.** The container can `git push` without ever holding a
  key: the host performs the filtered operation on its behalf.

## Why this way

Most isolation tools build an *image* and run it under a *daemon* as *root*. That buys strong
multi-tenant security, and it costs a build step, a copy of your files, a background service,
and seconds of startup.

A coding agent doesn't need any of that. It needs its own blast radius and a fast loop. So a
`tup` container is just **namespaces laid over your real working tree** — no image, no daemon,
no `sudo`. Binaries live once in a shared read-only `/nix/store`, so per-project overhead is
near zero and a container starts in ~10 ms. The agent edits *your* files directly; there is
nothing to sync or rebuild.

| | typical container | `tup` |
|---|---|---|
| Needs | image build, daemon, root | nothing but user namespaces |
| The agent's files | a copy inside the image | your actual working tree, bind-mounted |
| Startup | seconds | ~10 ms |
| Move a running session | checkpoint tooling, if at all | `tup move`, addresses preserved |

## Install

Linux with unprivileged user namespaces. Build with Cargo:

```sh
cargo build --release          # binary at target/release/tup — put it on your PATH
```

Optional, per feature: `pasta` (for `--net pasta`), `nsenter` (for `tup attach`), `nix`
(for `--env nix`), `rsync`+`ssh` (for `tup move`).

## Commands

| Command | What it does |
|---|---|
| `tup run [flags] <project> [-- <cmd>]` | Run one command in a fresh namespace. |
| `tup up [flags] <project>` | Launch a PID-1 supervised container that starts declared services. |
| `tup ps` | List projects: state, PID, net mode, env, service count. |
| `tup attach <project> [-- <cmd>]` | Join a running container (a shell by default). |
| `tup stop <project>` | Cleanly stop a running project. Idempotent. |
| `tup rm [--force] <project>` | Delete a project's state. Dry-run unless `--force`. |
| `tup cp <src> <dst>` | Copy files in/out of the working tree (`<project>:<path>` ⇄ host). |
| `tup logs <project> [-f]` | Show (or follow) a detached session's log. |
| `tup env <project> [list\|add <p>\|rm <p>\|detect]` | Inspect/edit the Nix prologue selection; `detect` infers it from the tree. |
| `tup move [--ship-all] <project> <dest>` | Migrate a session to a `local:<name>` or `ssh` target. |
| `tup preflight <project>` | Report what a move will cost before doing it. |
| `tup gate <name> [args]` | Drive a declared gate; `tup push`/`tup fetch` are the git-gate aliases. |

Shared `run`/`up` flags: `--net pasta\|host\|none`, `--env host\|nix`, `--harden`,
`--bind P` / `--ro-bind P` (same-path), `--map HOST:CONTAINER[:ro]` (distinct paths),
`--port [tcp\|udp:]HOST:CONTAINER` (pasta forwarding), `-e KEY=VAL` (env injection),
`-w`/`--workdir P`.

### Networking

`--net pasta` *(default)* gives a private netns with NAT egress via pasta; `--net host`
shares the host network; `--net none` is loopback-only. The mode is persisted per project, so
a `tup move` restores it — a `--net none` session is never silently re-opened to egress.

### Environment

`--env host` *(default)* inherits the host `/usr` and `/etc`. `--env nix` is **hermetic**: the
project's `flake.nix` *is* the environment — host `/usr` is dropped and `PATH` is just the
flake profile's `bin`. The closure already lives in the shared `/nix/store`, so realizing it
copies nothing. It's **zero-config** for common stacks: drop a Rust/Python/JS project in and
`tup run --env nix <project>` auto-detects the language (`Cargo.toml`→rust, `package.json`→node,
`pyproject.toml`→python, …) and activates the matching prologue. Customize with
`tup env web add rust` (or `tup env web detect`), and declare the things a flake can't model as
plain data in `outputs.tup`:

```nix
tup = {
  net     = "pasta";
  persist = [ "/var/lib/postgresql" ];                # rw dirs that survive teardown + ride a move
  binds   = [ { host = "/data"; path = "/data"; ro = true; } ];  # host files/dirs to link in
  ports   = [ { proto = "tcp"; host = 8080; container = 3000; } ];  # pasta forward (like -p)
  gates.git = {                                       # see below
    remote = "git@github.com:you/repo.git";
    allow  = [ "push" "fetch" ];
  };
  move = { rebuild = [ "/work/node_modules" ]; };     # keep derived dirs off the wire on a move
};
```

## Using credentials without holding them

A container often needs to *use* a secret — `git push` over ssh — but should never be able to
*read* it. `tup` offers two ways to grant that, both **deny-by-default**: with nothing declared
in the flake, the container reaches no host credentials at all.

**Gates** give the container a *capability*, not a credential. It holds only a socket. The host
is the reference monitor: it reads the policy, filters the request, and runs the operation
itself with its own credentials. Inside the box the agent runs `tup push main`; the host
checks *operation allowed? ref allowed? no option injection?* and then runs `git push` to the
one remote the policy names. Nothing — not a key, not a token — ever enters the container, and
it can do *only* the declared operation to the declared remote.

Gates are extensible. Besides the built-in `git` gate, you can declare a **command gate** — a
fixed, flake-authored host program the container may invoke with an allowlisted argument and
nothing else (no shell, no option injection, the program and its cwd fixed by the policy):

```nix
gates.deploy = { kind = "command"; run = "/opt/bin/deploy"; allow = [ "staging" "prod" ]; };
```

Inside the container, `tup gate deploy staging` runs the host's `deploy staging` and relays the
result — the container never learns how deploy authenticates.

**Agents** forward a *socket to a host `ssh-agent`*, not the key. `agents.ssh.key = "<name>"`
runs a dedicated agent holding a tup-managed, repo-scoped deploy key; inside the container,
`git` signs GitHub's challenge over the socket. The private key never crosses the boundary, and
when the session ends the agent dies. Use this when the container must run `git` itself; prefer
a gate otherwise, since it's tighter — a gate authorizes an *operation*, an agent can sign
anything.

> This repository was first published through its own gate: a credential-less `tup push` inside
> the container drove a host gatekeeper that did the authenticated push.

## Backends

`tup` renders one backend-neutral container spec through either backend, and both emit the same
ordered mount list so they can't drift:

- **pyrex** *(default)* — raw namespace syscalls, no external helper. Owning the `fork` gives
  `tup` the sandbox PID 1's host pid directly. Falls back to bwrap automatically where
  unprivileged user namespaces aren't available.
- **bubblewrap** — shells out to `bwrap`. Select with `TUP_SANDBOX=bwrap`, or build a
  bwrap-only binary with `--no-default-features`.

`tup` assumes a **cooperative agent**: the isolation is there to catch mistakes, and egress is
open by design (the agent needs the model API and package mirrors). `--harden` drops all
capabilities and starts a fresh session to lower the stakes further.

## Project layout

Everything for a project lives under `~/.tup/<project>/`:

```
root/        the rw working tree (the agent's files)
services     the service manifest (start/stop directives)
net          the persisted net mode
init.pid     the sandbox PID 1 (pyrex backend)
persist/     backing dirs for declared persist volumes
env/         the Nix flake, prologue selection, synthesized /etc (--env nix)
```

## Learn more

[`DESIGN.md`](DESIGN.md) — the full model: the migration protocol, the backend seam, the Nix
integration, the capability model, and the decision log.
