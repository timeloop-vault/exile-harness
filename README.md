# exile-harness

A tool-grounded AI harness for Path of Exile 1 & 2. Instead of training game
knowledge into a model, `exile` gives the model tools: live league and economy
data, extracted game data, wiki retrieval, and Path of Building for build math.
The model reasons; the harness knows.

**Status:** milestone 4 — the full loop works: chat drives a configured
model (any OpenAI-compatible endpoint: Ollama, vLLM, OpenRouter), which
calls the harness tools and answers with cited sources. Copy
`exile.example.toml` to `exile.toml` to enable a model; without one the
REPL runs in tool-only mode (`/call league {"game":"poe1"}`).

## Quickstart

```
cargo run -p exile-cli
```

Inside the REPL: `/tools` lists available tools, `/call <name> <json>` runs
one directly, `/help` shows all commands, `/quit` (or `/exit`) leaves.

## Development

One-time setup after cloning:

```
git config core.hooksPath .githooks
```

The pre-commit hook runs `cargo fmt --check`, `cargo clippy -D warnings`
(pedantic), and `cargo test`.

## Design

- Core is a library; every frontend (CLI now, TUI/GUI/web/mobile later) is a
  thin shell rendering a typed event stream.
- Facts come from tools at runtime, never from prompts or weights.
- Game math comes from Path of Building, never from the model.

See [CLAUDE.md](CLAUDE.md) for the full architecture and project laws.
