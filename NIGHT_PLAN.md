# Overnight build plan (2026-11-24)

Branch `overnight` (worktree `.claude/worktrees/overnight`), off `main` @ 1c7915b.
Goal: build + test the discussed tups features. Work fans out to subagents in their
own worktrees off `overnight`; the orchestrator merges, integrates, tests, runs examples.

Toolchain here: nix @ `/nix/var/nix/profiles/default/bin/nix`, pasta @ `/usr/bin/pasta`,
userns on, egress ok, /nix/store 97G (rust cached). Rust build PATH:
`/nix/store/sq6f11hs89dfrn3j4jkb60mlxgrx5wjh-rust-default-1.98.0-nightly-2026-06-01/bin`.

## Streams
- [x] **move rollback** (done, commit 4862253) — transactional stage→swap→confirm→commit.
- [ ] **A — Nix first-class / zero-config** (branch `overnight-nix`): detect rust/python/js
      from marker files, auto-select prologues, examples that run with `--env nix`.
- [ ] **B — Escape hatches** (branch `overnight-escape`): local maps (`--map host:ctr`),
      port forwarding (`--port`), both on `run` + `up`, flake `outputs.tup.ports`.
- [ ] **C — Gates first-class** (branch `overnight-gates`): generalise beyond git —
      named gates, a generic command gate with arg allowlist, common built-ins, per-gate
      sockets, `tup gate <name>` driver (keep `tup push`/`fetch`).
- [ ] **D — Move rebuild-vs-rsync dial** (orchestrator, in migrate.rs): default-exclude
      derived/cache dirs (target/, node_modules/, .venv/…), flake `outputs.tup.move`
      override, preflight reporting, `--ship-all`.
- [ ] **E — Examples** (orchestrator): examples/{rust,python,node}, verified end to end.
- [ ] **F — Docker parity** (late): comparison doc + port high-value features
      (tup cp, -e env vars, tup logs, service restart policy, workdir on up).
- [ ] **G — Docs + report**: DESIGN.md, README, final summary.

## Conventions for parallel safety (to minimise nixenv.rs / run.rs / up.rs conflicts)
- Each stream adds its config reader as a NEW `pub fn read_<x>(project)` in nixenv.rs
  returning its OWN struct. Do NOT modify `TupConfig`/`read_tup_config`.
- The `FLAKE` template stanzas (ports/gates/move) are pre-added by the orchestrator;
  subagents do not edit the FLAKE string.
- In run.rs/up.rs, add spawn/apply logic as small additive hunks inside the
  `EnvMode::Nix` block; keep them self-contained.
- Every stream: `cargo build` + `cargo test` green before handing back; add unit tests.
