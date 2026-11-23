# tup — ultra-micro containers for coding agents

> Contain the blast radius of a coding agent, and move its session between servers —
> while sharing binaries and libraries across all projects.

## What it is, in one line

Guix-container-style isolation (shared `/nix/store` + per-project fs/net/pid namespaces),
**plus a declared service/lifecycle layer and between-turn state migration**, aimed at a
coding agent rather than a human. Each piece exists already (Guix `shell --container`,
Procfile/systemd, agent-sandbox movers like Morph/Daytona, `mosh` roaming reattach); the
combination is the point.

## Goals & non-goals

- **Goal:** contain the blast radius of a coding agent — an agent's mistake stays inside its
  project (its files, its ports, its process tree).
- **Goal:** move a coding *session* to and between servers, transparently.
- **Goal:** near-zero **space** overhead and near-zero **marginal binaries** per project
  (shared store). *Not* zero startup/move time — namespace plumbing is small but nonzero.
- **Non-goal: this is not a security boundary.** We assume a **cooperative-but-fallible**
  agent. Permissions (`ro`/`rw` links, isolation) defend against *accidents and mistakes*,
  not a malicious escape. Do not market it as a sandbox against adversaries.

## Model

A **project** gets three namespaces and a shared store:

- **Dedicated filesystem root** (rw) — the mutable working tree + persist volumes.
- **Dedicated network namespace** — own loopback, so parallel agents never collide on ports
  and never accidentally reach each other's services. Internal addresses are **move-invariant**
  (a service is always `127.0.0.1:3000` on every server). Egress to the internet is **wide
  open** (the agent needs the model API and package mirrors). No egress policy.
- **Dedicated PID namespace** — the container's `init` is **PID 1**, so it *owns* the whole
  process tree: it can signal, quiesce, and reap everything cleanly. This is what makes
  "the container owns the processes" literally true.
- **Shared `/nix/store`** (ro, bind-mounted) — binaries and libraries are one copy, referenced
  by every project. ~0 marginal bytes.

### Environment = a Nix flake (no second config layer)

The env *is* the project's `flake.nix`. `flake.lock` pins by hash, the store dedups, a binary
cache ships paths by hash to other servers. We do **not** build an abstraction over flakes —
agents author the flake, the user never has to read it. The only thing a flake doesn't model
(links / network / persist) lives as **plain data in a flake output: `outputs.tup = { ... }`**.
One source of truth, one evaluator (Nix), no second format.

**Implemented — pieces 1–3, hermetic (`src/nixenv.rs`).** `tup run --env nix <project>` (also
`tup up --env nix`) makes the environment the project's flake, fully hermetic:

- *(piece 1)* The flake lives at `~/.tup/<project>/env/flake.nix`. tup builds its `tupEnv`
  output (a `buildEnv`) host-side via `nix build`, pinning a `flake.lock`, and resolves the
  `profile` out-link to a store path. The sandbox then **drops the host `/usr`/`/etc`
  entirely**. The closure is already in the shared ro `/nix/store` bind, so realising the env
  copies nothing; it only adds a synthesized `/etc` (passwd/group/resolv.conf) and symlinks the
  two paths the world hardcodes — `/bin/sh` and `/usr/bin/env` — into the profile. PATH is just
  `<profile>/bin`; the host env is wiped (`--clearenv` / explicit clear) and replaced with
  PATH + HOME + SSL_CERT_FILE. Lives behind the `env` field on `Spec`, so **both backends
  render it** (verified: bwrap and pyrex produce an identical hermetic root).
- *(piece 2 — prologues)* The env is composed from named, reusable profiles: a generic `base`
  every project inherits, plus opt-in language prologues (`rust`, `python`, `node`, `go`, `c`,
  `lean`). The registry lives in `prologues.nix` and the selection in `active.nix` (both
  tup-owned data the stable `flake.nix` imports), so the flake is generated once and then the
  user's to edit, while `tup env <project> [list|add|rm]` manages the selection without ever
  rewriting the flake. Sharing across projects is automatic via store dedup — no shared flake.
- *(piece 3 — `outputs.tup`)* The things a flake doesn't model — network, persist volumes, and
  host binds ("links") — live as plain data in the flake's `tup` output (`{ net; persist;
  binds; }`). tup reads it with `nix eval --apply` (no JSON dependency) and applies it: `net`
  sets the default network mode (an explicit `--net` still wins) and `binds` become sandbox
  binds. Verified: a flake declaring `net = "none"` + a host bind is honoured by `tup run` with
  no CLI flags.
- *(piece 4 — persist + move transport)* `outputs.tup.persist` lists rw dirs that survive turns
  and ride the move payload; each is backed by `~/.tup/<project>/persist/<slug>` and bound rw.
  `tup move` now ships the flake (so the target realises the same env) and `nix copy --to
  ssh://<dest>` the built closure by hash (dedup; only missing paths transfer; a failure falls
  back to a target-side rebuild). Persist dirs rsync alongside the working tree, and the target
  resumes with `--env nix`. Because the supervisor (`tup`) is a glibc binary that must run as
  PID 1 *inside* the hermetic root, `tup up --env nix` binds tup's own loader (at its ELF-interp
  path) + libc/libgcc (into `/usr/lib`, a default search dir) — the only host files in an
  otherwise hermetic root, and only for the supervisor, never on a plain `tup run`. Verified:
  persist survives a local move + resume on both backends; `nix copy` ships the closure (the
  `ssh://` hop itself needs a second host, untestable here).

### Prologues

A **prologue** is a named, reusable, shared Nix profile of tools/libs:

- one **generic base prologue** every project inherits (git, ripgrep, coreutils, the agent, …),
- **language prologues** (`rust`, `python`, `lean`, …) a project opts into.

Prologues are **pinned per project by hash**; "shared across all projects" happens
automatically via content-addressed dedup (project A on `rust@v1` and B on `rust@v2` share
every common byte). Updates are an explicit per-project bump. A fleet-wide "bump all" command
is a convenience layer on top, not the default.

### The three levers for dependencies (keep lightness)

When a project needs something beyond the prologue, there are three ways to get it and only two
keep the lightness/migration guarantees. The tooling must make the good two the path of least
resistance:

| Need | Lever | Movable? |
|---|---|---|
| Toolchain / system lib (compiler, libpq, openssl) | add to the **flake/prologue** | ✅ shared, by-hash |
| Language library (pip/npm/cargo dep) | the **package manager → content-addressed shared store** (pnpm/uv/cargo cache) | ✅ shared |
| Imperative install into the mutable layer (`pip install --target ./x`, `curl > /usr/local/bin`) | **allowed but marked** | ⚠️ heavier to move |

Classification of derived state (`node_modules/`, `target/`, caches) is **inferred by default**
(git-tracked+pushed → fetch; known cache dirs → shared store / rebuild; everything else → ship);
the manifest only **overrides**.

### Typed links ("link other files with permissions")

A project may link paths into its root. Links are **typed by target** so movability is visible:

1. **Store path** (content-addressed) → fully movable, fetched by hash. ✅
2. **Another tup project** (by project identity, not a fs path) → a first-class, movable
   inter-project dependency; also how two sessions share. Links a **declared output** of the
   producer, *not* its raw working tree (so a move doesn't drag two mutable trees in lockstep).
3. **Arbitrary host path** (`/data/corpus`) → allowed, but **marks the project host-pinned**
   (won't transparently move unless the target exists on the destination). Explicit, never a
   silent dangling link.

Two permission axes: **`ro` (default) / `rw` (opt-in)** and **movable / host-pinned**.

## Migration

**Between agent turns only. No CRIU. No process/RAM freezing.** A coding agent is a
request/response loop; the natural safe point is "idle, waiting for the next instruction."
At that instant the resumable state is small:

- the **conversation** (the agent flushes it to the persist volume on its checkpoint hook),
- the **working tree** + uncommitted/untracked changes,
- the **flake ref** (rebuilds/fetches the env identically on the target).

### Move protocol

```
1. Reach an agent turn boundary (quiesce point).
2. PID 1 quiesces the process tree:
   - declared services  → run their clean `stop` / `checkpoint` hooks
   - undeclared processes → SIGTERM → wait → kill  (ephemeral, no state promise)
3. Snapshot persist volumes + working-tree delta.
4. Ship:  store paths by hash (fetched on target, never sent) + rsync the mutable delta over SSH.
5. On target: `tup up`  → fetch/rebuild env by hash, restore persist, relaunch declared services.
6. Agent resumes its loop (sees nothing). User's roaming client reattaches (mosh-style blip).
```

Transparent resume is **conditional on caches being networked**, not merely content-addressed:
if a derived-state cache (sccache/pnpm/binary cache) is cold on the target, the move is
honest-but-slow (a visible rebuild). "Movable" ⇒ the cache is a reachable networked store.

### Move preflight

Before any move, report what will be **lost or heavy**:
undeclared processes running · host-pinned links · un-shared mutable bytes to ship ·
caches not reachable from the target. One honest "here's the cost of this move" screen.

## Session shape (Model A)

The **agent runs inside** the container as a declared service; its conversation-flush is its
`checkpoint` hook, so conversation state rides in the persist volume. **A session is one
mobile object** — move = relocate one container. **Tool execution is always inside** the
container (edits, shell, builds — that's the containment). The **user attaches from outside**
via a roaming client that reconnects after a move (the session persists; the client
re-attaches; the user sees a ~1s blip, the agent sees nothing).

Deferred — **Model B** (one central/roaming agent brain attaching to many mobile projects) can
be added later without changing the container.

### Agent integration contract (small, cooperative)

Transparency to the *user* requires light cooperation from the *agent*:

- **quiesce** at a turn / tool-call boundary when asked,
- **flush** conversation state to the persist volume (its `checkpoint` hook),
- **resume** from that state on start.

Agents that honor it migrate fully. Agents that don't still get env-mobility (rebuild + working
tree), just not conversation continuity. This contract is the sharpest adoption risk — confirm
the target agent can honor it.

## Components & stack

- **`tup` CLI** (Rust, single static binary) — verb surface: `init`, `up`, `run`, `ps`,
  `move`, `attach`, `preflight`.
- **`tup init`** — the in-container **PID 1 / supervisor** (same binary, different mode). Holds
  the namespaces open, starts/reaps declared services, handles the quiesce signal. *This is the
  runtime.* It is per-container, **not** a system daemon.
- **No persistent host daemon** (MVP). Migration = **SSH + rsync**; the target runs `tup up`.
  Roaming reattach = `nsenter` / ssh reconnect. A host daemon only earns its place later for
  fleet/discovery (Model B).
- **Isolation:** `bwrap` (rootless user+pid+mount ns + binds) + **`pasta`**/`slirp4netns`
  (rootless netns with NAT egress + own loopback).

### Backends (the sandbox seam)

The container spec is backend-neutral: `sandbox::Spec` + the ordered `Op` list from
`base_ops` describe *what* to build; a backend decides *how*, rendering the **same** ops so
the two can't drift.

- **`bwrap`** (default) — renders ops to bubblewrap flags. Proven, audited mount handling.
- **`pyrex`** (cargo `--features pyrex`, runtime `TUP_SANDBOX=pyrex`) — a small, cooperative,
  in-house namespace setup (unshare user/pid/mount/ipc/uts, flag-preserving ro binds,
  pivot_root, exec). **Not** a security sandbox (no symlink-safe mounts, no seccomp, weaker
  cap scrub) — the point is that owning the `fork` yields the sandbox PID 1's host pid for
  free, written to `~/.tup/<project>/init.pid`. That removes the `/proc` cmdline scan the
  bwrap backend needs and lets tup own the whole death chain. Verified end-to-end for
  `--net none`/`--net host` and the `tup up` supervisor (pidfile + clean quiesce); `--net
  pasta` shares the bwrap path's `wrap_pasta` and is untested only because it can't run
  nested inside a pasta'd session. Pitched as the eventual default once build+test in the
  target uid/systemd setup is routine; see the review notes for why (pidfd, seccomp fd,
  die-with-parent chain).
- **Env:** Nix (store + flakes), binary cache; language caches (pnpm/uv/cargo, sccache) for
  by-hash derived state. *(Nix integration is a later slice; the namespace/move machinery does
  not depend on it.)*
- **Transport:** `rsync` over `ssh`.

### Identity

`project identity = flake identity (env, by hash) + a stable instance id (the mutable volume)`.
Do **not** conflate the env hash with the session identity — two checkouts of one flake are two
sessions.

## Build plan — prototype the one unproven claim first

Everything except the move round-trip is assembling known-good parts. Prove the thesis first:

> A dummy agent (`loop: read instruction → edit a file → idle`) running as PID 1's child in a
> tup container with a shared store bind-mounted and its own netns, plus **one declared
> service** (a trivial HTTP server holding a counter in a persist file). Then
> `tup move host-A → host-B`: quiesce at the loop boundary, rsync persist + working tree,
> relaunch the service, roaming client reattaches.
>
> **Question it answers:** does the agent resume with zero visible loss, and does the user's
> blip feel like mosh-roaming (fine) or a dropped session (fatal)? If transparent, the thesis
> holds and the rest is engineering.

### Increments

1. **`tup run <project> -- <cmd>`** — assemble a namespace (shared ro system layer + dedicated
   rw root + pid ns) and run a command inside. *(this commit)*
2. **`tup init` as PID 1** — supervisor, declared services, clean quiesce/reap.
3. **Network** — `pasta` wiring: own netns, loopback, NAT egress.
4. **Persist volumes + working-tree model** — the move payload boundary.
   *(persist volumes done via `outputs.tup.persist` — see slice 7 piece 4.)*
5. **`tup move` + `tup preflight`** — the between-turn migration round-trip over SSH.
6. **`tup attach`** — roaming client reattach.
7. **Nix env integration** — flake eval → store paths → bind-mount; prologues.
   *(pieces 1–4 done: hermetic `--env nix` — flake → profile → PATH is just the profile, host
   /usr dropped; prologues (base + opt-in languages, `tup env`); `outputs.tup` net/persist/binds;
   `tup move` ships the flake + `nix copy`s the closure by hash and restores persist volumes.)*

## Decision log (what the grilling settled)

| # | Decision |
|---|---|
| Q1/Q2 | Between-turn migration only; **no CRIU**. State = conversation + working tree + flake ref. |
| Q3 | Derived state **content-addressed & shared**; **infer by default**, manifest overrides. |
| Q4 | Env = **flake**; container config = **`outputs.tup`** data; no second format. Shared base prologue + language prologues. |
| Q5 | **Pin prologues/deps per project by hash**; dedup provides "shared"; fleet-bump is a convenience. |
| Q6 | Per-project **netns**: sibling isolation + move-invariant addressing; **egress open**; no policy. |
| Q7 | **Typed links** (store / tup-project / host-pins); `ro` default, `rw` opt-in; accidents not malice. |
| Q8 | Common tools in the prologue; the real rule is the **three-lever discipline** (avoid imperative-into-mutable). |
| Q9 | Undeclared processes **ephemeral** (SIGTERM→wait→kill) with a **move-preflight warning**; declared services use hooks; durable state in persist. |
| Q10 | **Model A** — agent inside, session = one mobile object, user reattaches. Model B deferred. |

## Security posture & hardening

tup is a blast-radius boundary, **not** a security sandbox (see Goals & non-goals).
Measured on a root-mapped deployment: no seccomp, full caps in the userns, container-root
mapped to host-root (`uid_map 0 0 1`). What that contains vs. doesn't:

- **Contains:** unbound host paths, host processes (pid ns), host ports/loopback (net ns);
  writes outside bound paths are ephemeral. `NoNewPrivs=1` blocks setuid escalation.
- **Does NOT contain (adversarial):** kernel escape via the unfiltered syscall surface
  (as real root where root-mapped); resource-exhaustion DoS (no cgroup caps); exfiltration
  (egress is open by design); tampering with bound-rw paths — the project, `~/.claude`
  (including `.credentials.json`), and the grovy control socket.

`--harden` (opt-in on `tup run`/`tup up`) applies the two always-correct bwrap flags:
- `--cap-drop ALL` — the sandboxed process keeps no capabilities, so an escape lands with
  an empty cap set even when root-mapped (the highest-value, zero-cost mitigation).
- `--new-session` — `setsid`, blocking TIOCSTI terminal injection into the parent.

**Planned hardening** (deliberately not shipped blind — each needs in-container build + test
in the target's uid/systemd setup):
- **non-root uid** (`--uid`/`--gid`) — needs a userns uid map wider than the single mapped
  uid; environment-dependent, and bound-rw files must be owned by that uid.
- **seccomp** (`--seccomp`) — needs a BPF profile, and since pasta does not pass fds to its
  child, bwrap's `--seccomp FD` only works under `--net host|none` without extra plumbing.
- **cgroup `memory.max`/`pids.max`** — cleanest via `systemd-run --scope -p MemoryMax/TasksMax`
  rather than manual cgroup v2 writes (the no-internal-processes rule bites under a managed
  scope); depends on the target's systemd/delegation.
