# Running grovy sessions inside tups

grovy launches each Claude session in its own tmux session as:

    $GROVY_CLAUDE --settings <file> [extra…]

in the project directory, with `GROVY_SESSION`, `GROVY_STATE_DIR`, `PYTHONPATH`
(and a few others) in the environment. `$GROVY_CLAUDE` defaults to `claude`.

So no grovy code changes are needed: point `GROVY_CLAUDE` at `tools/tup-claude`,
which runs the real `claude` inside a `tup` container.

## Why it works

grovy's hooks talk to grovyd over a **Unix socket in the state dir**, not over the
network. A Unix socket is a filesystem object, so binding `GROVY_STATE_DIR` into the
container lets the hooks reach grovyd even though the container has its own network
namespace. `tup` gives the session pasta egress for the model API. Proven end to end:
a containerized process reaches grovyd's socket across the netns.

The wrapper binds, at their real paths: the project tree (rw), `~/.claude` (rw),
`GROVY_STATE_DIR` (rw), the `claude` binary's dir (ro), and the `PYTHONPATH` dirs (ro).

## Enable (opt-in, reversible)

`GROVY_CLAUDE` affects **all new sessions** while it is set; sessions already running
are untouched. To try it:

1. Build tup:  `cd tups && cargo build --release`  (binary at `target/release/tup`).
2. Put `tup` on grovyd's PATH, or pass `TUP_BIN=/abs/path/to/tup` in the env below.
3. Set the override in grovyd's environment and restart it. For the systemd user service:

       systemctl --user edit grovy
       # add:
       [Service]
       Environment=GROVY_CLAUDE=/abs/path/to/tups/tools/tup-claude
       Environment=TUP_BIN=/abs/path/to/tups/target/release/tup

       systemctl --user restart grovy      # running sessions keep going

   Or, for a quick one-off without the service, run grovyd by hand with the env set.
4. Start a session from the phone as usual. It now runs inside a tup container — check
   with `tup ps` (coming) or `pgrep -af '/run/tup init'`.

To revert: remove the `Environment=` lines (or unset the vars) and restart grovy.

## Env knobs (read by `tools/tup-claude`)

| var | meaning | default |
|---|---|---|
| `TUP_BIN` | path to the `tup` binary | `tup` on PATH |
| `TUP_REAL_CLAUDE` | the real claude binary | `claude` on PATH |
| `TUP_NET` | `pasta` \| `host` \| `none` | `pasta` |
| `TUP_SESSION_BINDS` | extra binds, e.g. `--bind /data --ro-bind /opt/x` | — |

If a session can't start, the usual cause is a path claude needs that isn't bound
(a toolchain under `$HOME`, a cache dir). Add it via `TUP_SESSION_BINDS` — the error in
`tup ps`/the tmux pane will name the missing path. This is the "host-pinned" trade-off
from DESIGN.md surfacing: the more real host paths a session binds, the less portable it is.

## Moving a grovy session (not yet)

`tup move` ships `~/.tup/<project>/root`. A grovy session's files live at their **real**
host paths (bound in), not under that root, so moving one means shipping the real project
dir plus the relevant `~/.claude/projects/<hash>` transcript — the host-pinned link case
(DESIGN.md Q7). That's the next piece of integration, after confirming sessions run well
containerized.
