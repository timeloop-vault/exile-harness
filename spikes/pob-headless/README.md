# Spike: headless Path of Building — both games (issue #19)

**Outcome: proven.** A stock, unmodified checkout of each engine runs
headless on Windows under a standalone LuaJIT, driven by one shared
JSON-lines stdio adapter (`adapter.lua`, ~230 lines, our code). Verified
2026-08-15:

| | poe1 (PathOfBuilding) | poe2 (PathOfBuilding-PoE2) |
|---|---|---|
| Engine version reported | 2.67.2 | 0.23.1 |
| Build import | 3.13 fixture XML → **build code → fresh process → correct stats** (Life 6728, CombinedDPS ~975k) | engine-generated build code → fresh process → correct stats (Life 65, base crit ×2 — visibly the PoE2 engine) |
| Round trip | `loadXML → makeCode → loadCode → stats` | same, zero per-engine code |

Run it: `.\demo.ps1 -Game poe1 -BuildXml ..\..\vendor\pob\poe1\spec\TestBuilds\3.13\OccVortex.xml`
and `.\demo.ps1 -Game poe2`.

## Setup

```
git clone https://github.com/PathOfBuildingCommunity/PathOfBuilding.git      vendor/pob/poe1
git clone https://github.com/PathOfBuildingCommunity/PathOfBuilding-PoE2.git vendor/pob/poe2
```

Checkouts are gitignored (`/vendor/`) and stay stock — never forked,
never patched (sanctioned-exception rule in CLAUDE.md).

LuaJIT 2.1, standalone (the checkouts ship the GUI runtime but no
standalone `luajit.exe`):

- Windows: `winget install DEVCOM.LuaJIT` (installs to
  `%LOCALAPPDATA%\Programs\LuaJIT\bin`)
- macOS: `brew install luajit` · Linux: distro package or luajit.org
- Plain Lua 5.1 does *not* work despite the wrapper's comment: the
  engine uses `bit` and `jit.opt.start`. The engines' own CI builds
  LuaJIT (see their `Dockerfile`), which also proves the headless path
  on Linux.

**Windows ABI note:** the engine's native modules (`lua-utf8.dll`,
required unconditionally at boot) import `lua51.dll` by name. They load
into a LuaJIT whose host *dynamically links* `lua51.dll` (DEVCOM's
does); a statically linked `luajit.exe` cannot load them. On Linux/mac
the same module comes from luarocks (`luautf8`) — their CI does exactly
that.

## How the adapter drives an engine

Everything below was pinned against both checkouts and cross-checked
with prior art (ianderse/pob-mcp's vanilla stdio bridge, upstream PR
PathOfBuilding#9505, maxrenke/pob2-mcp).

- cwd **must** be `<checkout>/src`: the wrapper stubs
  `GetScriptPath()`-style host functions to `""`, so every path resolves
  against cwd.
- `package.path` gets `../runtime/lua/?.lua;../runtime/lua/?/init.lua`
  (the exact value the engines' own `.busted` uses); `package.cpath`
  gets `../runtime/?.dll` for `lua-utf8.dll`. The adapter sets both
  itself — no environment needed.
- `dofile("HeadlessWrapper.lua")` boots the engine (`Launch.lua`,
  `OnInit`, one `OnFrame`) and exposes the driving surface as globals:
  `build`, `newBuild()`, `loadBuildFromXML(xml, name)`.
- Stats: `build.calcsTab:BuildOutput()` (when present), then read scalar
  fields off `build.calcsTab.mainOutput`. One `OnFrame` per mutation or
  the numbers are stale.

Protocol (one JSON object per line, engine noise on stderr, responses
on stdout after a `{"ready":true,...}` banner):

```
{"id":1,"cmd":"version"}                     → {"id":1,"ok":true,"result":{"engine":"2.67.2"}}
{"id":2,"cmd":"loadXML","xml":"<PathOf..."}  → load a build from XML
{"id":3,"cmd":"loadCode","code":"eNqt..."}   → import a base64url build code
{"id":4,"cmd":"makeCode"}                    → export the current build as a code
{"id":5,"cmd":"stats","keys":["Life"]}       → read mainOutput fields (default set if no keys)
{"id":6,"cmd":"new"} / {"cmd":"quit"}
```

## Findings the real tool crates must handle

1. **Build codes are dead headless out of the box.** `Inflate`/`Deflate`
   are SimpleGraphic *host* functions; both wrappers stub them to return
   `""`, so code import silently yields an empty build and export
   produces garbage. The adapter restores them with a LuaJIT FFI binding
   to zlib (the checkout's own `runtime/zlib1.dll` on Windows, system
   `libz` elsewhere) — codes then work fully in-engine, which no prior
   art achieves (pob-mcp is XML-only). The Rust crate should still own
   decode/encode as the primary path (`base64` url-safe + `flate2`,
   swapping `-_`→`+/`); the FFI route stays as the in-engine fallback.
2. **The two wrappers have structurally diverged** (poe1 dev:
   `_SimpleGraphic.def.lua` + `__mainObject__`; poe2: old inline stubs +
   `local mainObject`). Only the helper globals are a stable contract —
   never reach into wrapper internals.
3. **Boot failure blocks forever**: on startup error the wrapper prints
   `promptMsg` and calls `io.read("*l")`. A host must treat "no ready
   banner within timeout" as boot failure and kill the child.
4. **`CI` env var slows startup**: `CI=true` (set by every CI runner)
   disables `ModCache` loading and forces a full mod re-parse. Unset it
   when spawning.
5. **Keep one warm process per game.** Boot loads the full tree + mod DB
   (seconds, ~1GB RAM); a request/response loop over a long-lived child
   is the right shape — exactly what this adapter is. `newBuild()`
   between builds wipes the engine's FullDPS cache correctly.
6. **Per-game divergences**: XML `targetVersion` namespaces differ
   (`3_x` vs `0_x`) — never cross-feed codes between games;
   `loadBuildFromJSON` takes one arg on poe1, two on poe2; some
   `mainOutput` key names differ — the tool should request explicit
   per-game stat allowlists and validate against the GUI once per key.
7. **No network headless** (`LaunchSubScript` is a nil stub): account
   imports must be fed pre-fetched JSON. Fine — network belongs on the
   Rust side anyway (law: tools own I/O).
8. **Fixtures**: poe1 ships `spec/TestBuilds/*.xml` with expected-output
   `.lua` twins — ready-made regression material for the tool crate.
   poe2 ships none; engine-generated codes (this demo) fill the gap.
9. **Windows host gotchas** (bit us in `demo.ps1`): PowerShell 5.1
   `Get-Content -Raw` returns a decorated string that `ConvertTo-Json`
   serializes as an object — use `[IO.File]::ReadAllText`; and don't
   redirect the child's stderr in PS 5.1 (NativeCommandError wrapping).

## Path to the real `pob` tool

- One crate (`crates/exile-tools/exile-pob`), `game` parameter selects
  the checkout dir (law 3); spawns `luajit <adapter>` with cwd
  `<checkout>/src`, ready-banner timeout, idle timeout, and restart-on-
  desync — the same supervision shape `exile-llm` already uses for
  streams.
- Checkout location + pin: configurable path (gitignored `exile.toml`),
  documented clone commands, and a recorded engine commit + version in
  every tool response (`source` + `fetched_at` become
  `engine`+`engine_version` — provenance, law 1).
- Tool surface v1: import code/XML → stats. Later: item/tree/config
  mutations (pob-mcp's action vocabulary is a good design reference).
- Eval: re-point the parked `mechanics-two-more-math` question at the
  tool (grade that the model *calls* it rather than computing), and add
  a grounded DPS question against a pinned build code.
