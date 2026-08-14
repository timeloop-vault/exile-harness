# exile-harness

Tool-grounded AI harness + agents for Path of Exile 1 and 2. The harness
(tools, data, eval) is the product; agents are thin prompt/orchestration
consumers of it. CLI binary name: `exile`.

## Project laws

1. **No game facts in prompts, code, or this file.** League names, patch
   numbers, prices, mechanics — all of it comes from tools at runtime,
   stamped with `source` + `fetched_at`. Hardcoded facts rot; this killed
   the predecessor projects twice (once as LoRA weights, once as prompt
   text in poe-agents).
2. **The model never computes game math.** Build numbers come from the Path
   of Building engine (stock checkout, driven headless). The LLM only
   synthesizes.
3. **PoE 1 and PoE 2 are first-class.** Every tool takes a `game` parameter
   (`poe1` | `poe2`); corpora stay separated to avoid cross-game
   contamination.

## Constraints

- Backend/core is Rust: edition 2024, clippy pedantic, `unsafe_code = forbid`,
  lints defined once in `[workspace.lints]`.
- Own tooling over community dependencies. Community MCP/agent projects are
  learning material, not deps. Sanctioned exception: the maintained Path of
  Building community forks, driven as stock checkouts so we never maintain a
  fork ourselves.
- Platform-agnostic: everything must work on Windows, Linux, and macOS —
  no hardcoded machine paths anywhere; hooks stay POSIX shell (Git Bash
  covers Windows).

## Architecture

```
crates/
  exile-core/      agent loop, sessions, event stream — no I/O assumptions
  exile-tool-api/  Tool trait, registry, JSON-schema parameter types
  exile-llm/       OpenAI-compatible client (vLLM, Ollama, OpenRouter)
  exile-cli/       frontend #1, bin name: exile
  exile-tools/     one lib crate + thin CLI per tool     (from milestone 3)
prompts/           agent definitions                     (from milestone 5)
eval/              regression questions + runner         (from milestone 6)
```

- Dependency direction flows inward only: frontends depend on core, tools
  depend on tool-api. Nothing depends on a frontend.
- Core output is a typed event stream (`TokenDelta`, `ToolCallStarted`, …);
  frontends are renderers of it. This is what keeps CLI/TUI/web/mobile and
  local-vs-cloud composition cheap.
- Data freshness tiers: **A** per-patch versioned extracts (DAT/GGPK),
  **B** cached corpora with vintage stamps (wiki, patch notes, guides),
  **C** always live (leagues, prices, meta) — never cached beyond minutes,
  never written into prompts.

## Commands

- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all --check`
- Hooks (once per clone): `git config core.hooksPath .githooks`

## Workflow

- `main` is protected; every change lands via a milestone-sized PR for
  review.
- Use the `gh` CLI for GitHub operations. If it is not on PATH, search the
  platform's usual install locations or ask the user where it lives.
