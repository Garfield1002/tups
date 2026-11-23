# tup examples — zero-config Nix environments

Each example is an ordinary project. With `--env nix`, tup reads the project's
flake for its environment; a fresh project with no flake gets one scaffolded, and
tup **auto-detects** the language from marker files and activates the matching
prologue — so these run with no hand-written Nix config.

| Example | Marker | Auto-activated prologue | Tools |
|---|---|---|---|
| `rust/` | `Cargo.toml` | `rust` | rustc, cargo, rust-analyzer, gcc, pkg-config |
| `python/` | `pyproject.toml` | `python` | python3, uv, ruff |
| `node/` | `package.json` | `node` | nodejs, pnpm |

## Run one

A project's working tree lives at `~/.tup/<project>/root` (mounted as `/work`).
Copy an example in, then run it hermetically:

```sh
proj=demo-rust
mkdir -p ~/.tup/$proj/root
cp -r examples/rust/. ~/.tup/$proj/root/
tup run --env nix $proj -- cargo run        # auto-detects Cargo.toml → rust prologue
```

```sh
proj=demo-node
mkdir -p ~/.tup/$proj/root
cp -r examples/node/. ~/.tup/$proj/root/
tup run --env nix $proj -- node index.js    # auto-detects package.json → node prologue
```

```sh
proj=demo-python
mkdir -p ~/.tup/$proj/root
cp -r examples/python/. ~/.tup/$proj/root/
tup run --env nix $proj -- python3 main.py  # auto-detects pyproject.toml → python prologue
```

Detection is a no-op once you customise the prologue selection (`tup env <p> add/rm`);
run `tup env <p> detect` to re-apply it explicitly. The first `--env nix` build
realises the flake (may fetch from the binary cache); later runs reuse the store.
