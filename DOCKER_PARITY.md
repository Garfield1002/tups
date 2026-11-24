# Docker parity

How Docker's concepts map onto **tups**, honestly. tups is not a Docker clone and does not
try to be — it is an *ultra-micro container for a coding agent*: a per-project
fs/pid/ipc/uts namespace plus a per-project netns, sharing one read-only `/nix/store`, with a
declared service/lifecycle layer and between-turn session **migration**. Two differences in
the model color the whole table, so read them first:

1. **The environment is a Nix flake, not an image.** There is no mutable image to `build`,
   `commit`, tag, or `push` to a registry. `~/.tup/<project>/env/flake.nix` + `flake.lock`
   pin the env by hash; the shared content-addressed store *is* the dedup/layer story; a Nix
   binary cache (and `nix copy --to ssh://` during a move) *is* distribution. "Rebuild the
   same env on another host, by hash" replaces "pull the image."
2. **It is explicitly not a security boundary** (see `DESIGN.md` — cooperative-but-fallible
   agent). `ro`/`rw` binds, the netns, and `--harden` reduce *blast radius* and the stakes of
   an accident; they are not claimed to contain a malicious escape. Docker makes a similar
   non-guarantee in practice, but tups says so out loud.

A third thing has **no Docker analog at all**: `tup move`/`tup preflight` relocate a live
*session* (working tree + persist volumes + flake-by-hash) between servers between agent
turns. That is the headline feature, and it is the reason several Docker knobs (`commit`,
`pause`, registries) are "out-of-model" rather than "not built yet" — they belong to a world
where the unit of shipping is a mutable image, and tups' unit is a flake plus a mobile
working tree.

Status legend: ✅ exists · 🟡 partial · 🔭 portable-future (fits the model, not built) ·
⛔ out-of-model (deliberately not a thing here).

## The table

| Docker | tups equivalent (CLI + flake `outputs.tup`) | Status | Notes |
|---|---|---|---|
| `docker build` / images | `flake.nix` (`packages.tupEnv` = `buildEnv`) built host-side by `nix build`; prologues (`base` + opt-in `rust`/`python`/`node`/…) compose the env; `tup env <p> list\|add\|rm` edits the selection. Zero-config: `--env nix` auto-detects prologues from marker files (`Cargo.toml`→rust, `package.json`→node…). | ✅ (different model) | Env is a flake pinned by hash, not a layered image. No `Dockerfile`; the flake is the build. |
| `docker run` | `tup run [--net][--env][--harden][--map][--port][--chdir] <p> [-- cmd]` — one-shot command in a fresh namespace. | ✅ | |
| long-running container | `tup up <p>` — launches the PID-1 supervisor (`tup init`) that owns the process tree and declared services. | ✅ | PID 1 = real init: signals, quiesces, reaps. |
| `docker exec` | `tup attach <p> [-- cmd]` — `nsenter`s into the live container's user/mount/uts/ipc/pid(/net) namespaces; clean per-env PATH; defaults to `/bin/sh`. | ✅ | Under `--net pasta` the attached shell shares the host net (pasta's netns isn't enterable from the inner userns); noted at attach time. |
| `docker cp` | **`tup cp <src> <dst>`** — one side is `<project>:<path>`. The `<project>:` side resolves under the writable root (`~/.tup/<p>/root`, the container's `/work`): relative → relative to `/work`, absolute must be under `/work`. Files and dirs (recursive); parents created on copy-in; `..`-escape and out-of-`/work` paths refused. | ✅ (**new tonight**) | Addresses the working tree directly on the host — no running container needed. |
| `docker logs [-f]` | **`tup logs [-f] <project>`** — prints `~/.tup/<p>/up.log` (what a *detached* `tup up`/`tup move` session and its services write); `-f` follows by polling. | 🟡 (**new tonight**) | A *foreground* `tup run`/`tup up` writes straight to your terminal, not a file — `logs` says so when there's no log. No per-service log demux (all services share the supervisor's stdout → `up.log`). |
| `-e KEY=VAL` (env vars) | **`-e KEY=VAL` / `--env-var`** on `run` and `up` (repeatable); applied after the built-in env so an explicit value wins, and carried across the pyrex re-exec. | ✅ (**new tonight**) | Named `-e`/`--env-var` to avoid the clash with `--env` (env *mode*: host\|nix). |
| `-w` / `--workdir` | **`-w` / `--workdir <path>`** on `run` and `up` (`run` also keeps `--chdir`). | ✅ (**new tonight**) | `attach` still fixes cwd to `/work`. |
| `-p` / `--publish` | `--port [tcp\|udp:]HOST:CONTAINER` (CLI) and `outputs.tup.ports = [{ proto; host; container; }]`. | ✅ | pasta only — `--net host` already shares ports, `--net none` has no egress; a port on a non-pasta net is dropped with a warning. |
| `-v` / `--volume` (binds) | `--bind P` / `--ro-bind P` (same path in/out), `--map HOST:CONTAINER[:ro]` (distinct), and `outputs.tup.binds = [{ host; path; ro; }]`. | ✅ | `ro` is the default posture; `rw` is opt-in (typed-links model). |
| `-v` named volumes | `outputs.tup.persist = [ "/state" ]` — rw dirs backed by `~/.tup/<p>/persist/<slug>`, bound rw, that **survive turns and ride the move payload**. | 🟡 (different model) | Persist volumes are move-aware, not a global named-volume registry; no `docker volume create/ls/prune`. |
| `--name` | The **project name is the identity** — every verb takes `<project>`; state lives at `~/.tup/<project>`. Validated (`ensure_valid_name`) before any path use. | ✅ (different model) | There is no anonymous/random-name container; a project always has a stable name. |
| `--rm` (auto-remove) | Containers are ephemeral by default: writes outside bound paths vanish on exit; `tup rm [--force] <p>` deletes a project's *state* (working tree + persist + env). | 🟡 (different model) | Ephemerality is the default, not a flag; `tup rm` is the explicit teardown and is dry-run unless `--force`. |
| `docker stop` | `tup stop <p>` — SIGTERM to PID 1 so `init` runs quiesce (stop hooks, then bounded TERM→KILL sweep), then waits; idempotent. | ✅ | |
| `docker start` | `tup up <p>` (re-launch). | 🟡 | No separate `start` verb; `up` is launch. |
| `docker restart` | stop + `up` (two verbs). | 🔭 | A one-shot `tup restart` would be a thin wrapper. |
| `--restart` policy | **`restart <name> no\|on-failure\|always`** in the service manifest; `init` respawns the service per policy, capped at 5 restarts / 10s so a crash-loop can't spin forever (then it gives up). | ✅ (**new tonight**) | Default `no` preserves the original reap-and-exit behaviour exactly. |
| `HEALTHCHECK` | — services declare `start`/`stop` hooks only; no health probe or health-gated readiness. | 🔭 | Fits as an `outputs.tup` service field + a probe loop in `init`. |
| networks (`docker network`) | Per-project **netns**: `--net pasta` (default; own loopback, NAT egress via pasta), `--net host`, `--net none`. Addresses are **move-invariant** (a service is `127.0.0.1:3000` on every host). Inter-project sharing is a typed **tup-project link** (a declared output of the producer), not a shared bridge. | 🟡 (different model) | Sibling isolation + move-invariance by design; egress is wide open by design (agent needs model API + mirrors), so no network policy, no user-defined bridges/overlays. |
| `--memory` / `--cpus` | — not shipped. `DESIGN.md` plans it via `systemd-run --scope -p MemoryMax/TasksMax` (cleaner than raw cgroup v2 writes under the no-internal-processes rule). | 🔭 | Depends on the target's systemd/delegation; deliberately not shipped blind. |
| `docker inspect` | **`tup inspect <p>`** dumps the resolved config (state, pid, net, env/prologues, binds, ports, gates, persist, move dial, services); `tup ps` lists all projects. | ✅ (**new tonight**) | Human-readable rather than JSON; a `--json` mode would be a small add. |
| `docker commit` | — no mutable image to freeze. The env is a flake by hash; "capture current state" = commit the flake / working tree, which is already content-addressed and movable. | ⛔ (out-of-model) | Committing a mutated rootfs would defeat the by-hash dedup/move guarantees. |
| `docker pause` / `unpause` | — no freeze. Migration is **between-turn**, not a CRIU/RAM freeze (a coding agent's safe point is "idle between turns"). | ⛔ (out-of-model) | `tup move`'s quiesce is the closest thing, but it stops cleanly rather than freezing. |
| labels | — none. | 🔭 | `outputs.tup.labels` + a column in `ps` would be trivial and fit the plain-data output. |
| registries / `push` / `pull` | Distribution is the **Nix binary cache** + `nix copy --to ssh://<dest>` of the built closure by hash during `tup move` (only missing paths transfer; dedup; rebuild fallback). Prologues/deps are shared across projects by hash. | 🟡 (different model) | ⚠️ **Name clash:** `tup push` / `tup fetch` drive the **git gate** (a filtered escape hatch — the host performs the git op with its own creds), nothing to do with image registries. |

### Escape hatches & credentials (no single Docker verb, worth noting)

| Concept | tups | Status |
|---|---|---|
| Brokered credentials | `outputs.tup.agents.ssh.key = "name"` forwards an **ssh-agent socket** (the key never enters the container) — deny-by-default, explicit grant. | ✅ |
| Filtered host operations | **Gates** (`outputs.tup.gates`): the container holds only a socket; the HOST runs the op and enforces policy. Built-in `git` gate (`tup push`/`tup fetch` with `allow`/`refs`) + user-defined `command` gates (one allowed arg, run in a tree). Driven by `tup gate <name> …`. | ✅ |
| Move cost preview | `tup preflight <p>` — reports undeclared processes, host-pinned links, un-shared bytes to ship, cold caches. No Docker analog. | ✅ |
| Move dial | `outputs.tup.move = { ship; rebuild; }` — force dirs into the rsync payload or out to a target-side rebuild (defaults infer `target/`, `node_modules/`, `.venv/`… as rebuild). | ✅ |

## What we ported tonight

- **`tup cp`** (`src/copyfile.rs`, `cp` arm in `main.rs`) — `docker cp`-style host↔working-tree
  copy, addressing `~/.tup/<project>/root` as the container's `/work`. Files and directories
  (recursive), parents created on copy-in. Safety: the project name is validated; a container
  path outside `/work` is refused ("only `/work` is writable/real"); and any `..` that would
  climb out of the root is refused by a lexical resolver (`resolve_in_root`, unit-tested — it
  doesn't trust the filesystem, so it's safe for copy-in destinations that don't exist yet).
- **`tup logs [-f]`** (`src/status.rs`, `logs` arm in `main.rs`) — prints a detached session's
  `up.log`; `-f` follows by polling for appended bytes (Ctrl-C-able, and tolerant of a closed
  pipe). When there's no log it explains that a foreground `tup run`/`tup up` writes to the
  terminal, not a file.
- **`-e KEY=VAL` / `--env-var`** on `run` and `up` — env-var injection (the Docker `-e`),
  applied after the built-in env and carried across the pyrex re-exec via `--set-env`.
- **`-w` / `--workdir`** on `run` and `up` — set the container working directory.
- **`restart <name> no|on-failure|always`** service directive — `init` respawns per policy with
  a crash-loop cap; default `no` is the original behaviour.
- **`tup inspect <p>`** — a readable dump of a project's resolved configuration.
- This doc.

## Worth porting next (prioritized)

1. **`HEALTHCHECK`-style service probes** (`outputs.tup` service field + a probe loop in
   `init`; health-gate readiness and the move quiesce). Pairs with the restart policy now in
   place.
2. **Resource limits** (`--memory`/`--cpus` via `systemd-run --scope`). Already scoped in
   `DESIGN.md`; the real blast-radius win for a runaway agent (DoS is explicitly *not*
   contained today).
3. **`outputs.tup.env`** — declare env vars in the flake (the complement to the `-e` CLI flag),
   so a project's environment travels with it on a move.
4. **`tup restart`** and a **`tup start`** alias — thin ergonomic wrappers over stop+`up`.
5. **`outputs.tup.labels`** + a `ps` column — trivial, and useful once there are many
   projects/fleets.
6. **`tup inspect --json`** and **`--workdir` on `attach`** — small ergonomic/tooling rounding-out.
