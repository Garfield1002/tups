# Overnight build plan (2026-11-24)

Branch `overnight` (worktree `.claude/worktrees/overnight`), off `main` @ 1c7915b.
Goal: build + test the discussed tups features. Work fans out to subagents in their
own worktrees off `overnight`; the orchestrator merges, integrates, tests, runs examples.

Toolchain here: nix @ `/nix/var/nix/profiles/default/bin/nix`, pasta @ `/usr/bin/pasta`,
userns on, egress ok, /nix/store 97G (rust cached). Rust build PATH:
`/nix/store/sq6f11hs89dfrn3j4jkb60mlxgrx5wjh-rust-default-1.98.0-nightly-2026-06-01/bin`.

## Streams
- [x] **move rollback** (done, commit 4862253) — transactional stage→swap→confirm→commit.
- [x] **A — Nix first-class / zero-config** (merged): detect.rs infers prologues; auto-activate
      on `--env nix`; `tup env detect`. VERIFIED end-to-end: rust, node, python all run.
- [x] **B — Escape hatches** (merged 85265aa): `--map host:ctr[:ro]`, `--port [proto:]h:c`
      on run+up, flake `outputs.tup.ports`. e2e port-forward + map verified by subagent.
- [x] **C — Gates first-class** (merged 7cc0f3c): GateSpec/GateKind, command gates with arg
      allowlist, per-gate sockets under GATE_DIR, `tup gate <name>` driver, push/fetch kept.
- [x] **D — Move rebuild-vs-rsync dial** (merged 10b232c): default derived-dir excludes,
      flake `outputs.tup.move.{ship,rebuild}`, `--ship-all`, preflight reporting.
- [x] **E — Examples** (80eeaa0): examples/{rust,python,node} + README, all verified.
- [~] **F — Docker parity**: comparison doc + tup cp / tup logs (subagent) + -e env vars +
      --workdir on up (orchestrator). IN PROGRESS.
- [ ] **G — Docs + report**: DESIGN.md, README, final summary.

Integrated state on `overnight`: 66 unit + 3 integration tests, clippy clean.
Known follow-up: using a gate from inside a container needs the `tup` binary on the
container PATH (fine in `up` via /run/tup but not on PATH; not bound in `run`).

## Conventions for parallel safety (to minimise nixenv.rs / run.rs / up.rs conflicts)
- Each stream adds its config reader as a NEW `pub fn read_<x>(project)` in nixenv.rs
  returning its OWN struct. Do NOT modify `TupConfig`/`read_tup_config`.
- The `FLAKE` template stanzas (ports/gates/move) are pre-added by the orchestrator;
  subagents do not edit the FLAKE string.
- In run.rs/up.rs, add spawn/apply logic as small additive hunks inside the
  `EnvMode::Nix` block; keep them self-contained.
- Every stream: `cargo build` + `cargo test` green before handing back; add unit tests.
