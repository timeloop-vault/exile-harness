# exile-pob

The pob tool family, computed by the Path of Building engine (project
law 2 — build math never goes through the model), both games (law 3):

- `pob` — a build's current statistics from a share code or build XML.
- `pob_whatif` — the engine runs a build with and without hypothetical
  `custom_mods` lines and returns each stat's before/after/delta,
  precomputed (the ADR's workflow-shaped tool pattern).

All pob-family tools share warm per-game engine children through one
`EngineHost` — register them via `with_host(Arc<EngineHost>)` so N tools
still cost one engine per game.

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

- `src/lib.rs` — `EngineHost` (shared warm-engine supervisor), the two
  tools, per-game custom-mod injection (poe1: `CustomModifierBlock`
  into every config set; poe2: the legacy `customMods` input), and the
  game-vs-root check (poe1 `<PathOfBuilding>`, poe2 `<PathOfBuilding2>`
  — the engine silently loads a default build on a mismatch, so the
  tool fails loudly instead).
- `src/codes.rs` — share-code transform (url-safe base64 + zlib) on the
  Rust side; headless engines stub their own `Inflate`/`Deflate`.
- `src/bridge.rs` — child supervision: ready-banner and per-request
  timeouts, `CI` env var stripped (it disables the engine's ModCache),
  kill-and-respawn on any protocol failure.
- `lua/adapter.lua` — the JSON-lines adapter (embedded at build time,
  materialized to a temp file at spawn); wipes the engine's global
  cache before every load, which the poe2 fork does not do itself.
  Protocol and engine findings: `spikes/pob-headless/README.md`.

Live verification (needs LuaJIT + both checkouts):
`cargo test -p exile-pob -- --ignored`
