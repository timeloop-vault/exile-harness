# exile-pob

The `pob` tool: build statistics computed by the Path of Building engine
(project law 2 — build math never goes through the model). One tool, both
games (law 3); each game gets its own warm headless engine child.

## Setup

The engines are **stock checkouts, never committed** (gitignored under
`/vendor/pob/`) and never forked or patched. Bootstrap them with the
built-in fetch (GitHub snapshot tarballs at an explicit ref — no git
needed, provenance recorded in `.exile-fetch.json`, reusable later for
bundling engines into a distributable):

```
cargo run -p exile-pob -- fetch            # both games @ dev
cargo run -p exile-pob -- fetch --game poe1 --ref <tag-or-commit> --force
```

or clone them yourself:

```
git clone https://github.com/PathOfBuildingCommunity/PathOfBuilding.git      vendor/pob/poe1
git clone https://github.com/PathOfBuildingCommunity/PathOfBuilding-PoE2.git vendor/pob/poe2
```

LuaJIT 2.1 standalone (the checkouts ship no interpreter):

- Windows: `winget install DEVCOM.LuaJIT`
- macOS: `brew install luajit` · Linux: distro package or luajit.org
- Non-Windows also needs `luarocks install luautf8` for lua 5.1 (the
  Windows checkout ships the dll; see the spike notes on the lua51.dll
  ABI).

Configuration is environment-based, no config file required:

- `EXILE_POB_ROOT` — checkout root (default `vendor/pob`, resolved from
  the working directory)
- `EXILE_LUAJIT` — interpreter path (default: `PATH`, then the winget
  install location on Windows)

## Shape

- `src/codes.rs` — share-code transform (url-safe base64 + zlib) on the
  Rust side; headless engines stub their own `Inflate`/`Deflate`.
- `src/bridge.rs` — child supervision: ready-banner and per-request
  timeouts, `CI` env var stripped (it disables the engine's ModCache),
  kill-and-respawn on any protocol failure.
- `lua/adapter.lua` — the JSON-lines adapter (embedded at build time,
  materialized to a temp file at spawn). Protocol and engine findings:
  `spikes/pob-headless/README.md`.

Live verification (needs LuaJIT + both checkouts):
`cargo test -p exile-pob -- --ignored`
