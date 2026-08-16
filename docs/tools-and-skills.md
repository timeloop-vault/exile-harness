# Tools × skills: the agent's capability architecture (spike #35)

Decision record, 2026-08-16. Sources: measured numbers from this repo,
vendor guidance (Anthropic tool-writing/agent-building docs, OpenAI
function-calling guide, Qwen function-calling docs), the agentskills.io
skill spec, and the papers cited inline.

## What we measured (this repo, exact runtime strings, tokens ≈ chars/4)

- Always-loaded overhead per request today: **~1,860 tokens** native
  mode (~1,950 prompted) — `prompts/exile.md` ~703 tokens (38%), the
  5-tool registry ~1,159 tokens (62%). Growth is linear at ~286
  tokens per tool of current average size.
- `pob` alone is 37% of registry cost; ~230 tokens across descriptions
  is *flow/policy* text (how to sequence calls, what not to compute)
  that **duplicates `exile.md` rules 1/2/4/6/7** — paid twice every
  request, and subtly-different phrasings of the same rule measurably
  confuse small models.
- **Splitting pob/wiki into 3 focused tools each raises registry cost
  14–37%** even with half-length descriptions: per-tool fixed cost
  (schema boilerplate, the repeated `game` enum, name framing)
  multiplies faster than prose shrinks. Splitting is a routing lever,
  never a context-cost lever.

## What the literature says (small local models, ~30B class)

- Qwen3-32B tool-calling is within half a point of the family flagship
  (BFCL v3: 70.3 vs 70.8) — the model class is not our bottleneck at
  this tool count. Five flat tools sit inside every published safe zone
  (OpenAI: <20 visible; measurable degradation starts ~10–15 for small
  models; catastrophic at hundreds — LongFuncEval, arXiv 2505.10570).
- **Multi-turn is the small-model cliff** (e.g. Qwen3-4B: 62% overall
  BFCL but 35% multi-turn). Every model-mediated hop is an error
  opportunity. Both vendors therefore endorse consolidating fixed
  sequences *into* tools ("combine functions that are always called in
  sequence" — OpenAI; "consolidate multi-step operations" — Anthropic).
- Tool **names** carry most of the selection signal for small models
  (Hammer, arXiv 2410.04587); descriptions should be a few precise
  sentences with when-to-call triggers, not essays. Enum-constrained,
  flat, union-free schemas are the cheapest accuracy win; schema unions
  (router tools with `command` fields) are where small models throw
  type/required-field errors.
- Small models are weakest at *not* calling a tool when none fits;
  evals need no-tool-applies probes.
- Long tool responses hurt more than tool count at our scale: answer
  extraction degrades 7–91% as responses grow 10K–80K tokens — keep
  results trimmed (we already do).

## Skills: what they are and when we add them

Prior art (Claude Code skills / agentskills.io spec; MCP prompts):
a skill is **context, not capability** — a named instruction pack whose
name+description is always loaded (~100 tokens) and whose body loads
only on invocation (progressive disclosure). Triggering is an ordinary
tool call over an advertised catalog — no retrieval magic. That means
our harness can implement skills **as a tool** with zero core changes:
one `skill` tool whose description lists skill names + one-liners and
whose `execute(name)` returns the body into the transcript.

Skills earn their keep when flow/reference material would otherwise sit
in always-loaded context. Today, after the decisions below, we have less
than one skill's worth of such material — so we pin the design and defer
the build until ≥2 real recipe documents exist.

## Decisions

1. **Flat registration stays; no router/mega-tool, no micro-splitting.**
   Wiki remains one tool (its two modes share one cheap schema). Revisit
   only if eval shows mode-selection errors, or past ~10 visible tools —
   at which point two levers exist, neither of which is splitting:
   *deferred/shortlisted exposure* (ties into #20), and **agent-level
   partitioning** — an orchestration layer where sub-agents own focused
   tool subsets and behaviors (OpenAI's guidance for large surfaces is
   exactly "role-specific sub-agents with 4–6 tools each"). Partitioning
   caps any one agent's always-loaded context but pays in latency and
   extra hops — and hops are the measured small-model cliff — so it fits
   when a flow justifies a dedicated agent, not as a default. The core's
   design anticipates it: sessions compose cheaply over the typed event
   stream, so an orchestrating agent driving specialist sessions is an
   arrangement of existing pieces, not new architecture.
2. **New capability lands as workflow-shaped tools** — the tool runs the
   fixed sequence internally and returns the synthesized result, keeping
   intermediate data out of context and cutting model hops:
   - `pob_whatif`: baseline + modified runs internally → returns a
     structured stat diff (law 2 made structural: the model cannot even
     be tempted to subtract).
   - #34 lands as `pob_compare` (equip candidate items in turn, return
     the diff), not as an `items` argument the model orchestrates.
   - Prefix families (`pob_*`) as tools are added; names maximally
     distinct; the `game` enum discipline extends to every closed value
     set.
3. **Deduplicate policy text.** Descriptions state WHAT a tool does and
   when to call it; conduct rules (cite sources, never compute, per-game
   separation) live once, in `prompts/exile.md`. Extract ~150 tokens of
   pob's policy prose; keep short cross-tool hints ("resolve the league
   id with the `league` tool first") — small models chain unreliably
   without them and they are cheap.
4. **Skills-as-a-tool is the pinned mechanism** (names+descriptions
   always loaded, bodies on demand, invocation = tool call), deferred
   until ≥2 recipe docs exist. First candidates: build-comparison
   walkthrough, trade/price investigation flow.
5. **Harness hardening backlog** (from the failure-mode literature):
   schema-validate every call in Rust and return terse, actionable
   errors (exists — keep); return all parallel tool results in one
   message (verify in exile-core); add a no-tool-applies eval probe and
   watch over-triggering; keep tool responses trimmed (exists).

## Consequences

- #34 (item comparison) is re-scoped to a workflow-shaped `pob_compare`.
- Follow-up issues carved from this record: `pob_whatif` +
  description dedupe; eval probes for no-tool-applies/over-triggering;
  skills mechanism (deferred, design pinned above).
- The roadmap's future tool crates (patch-notes, gamedata, trade) inherit
  rules 1–3: workflow-shaped, distinct names, policy-free descriptions.

## References

Vendor guidance:

- Anthropic, *Writing effective tools for agents* —
  <https://www.anthropic.com/engineering/writing-tools-for-agents>
- Anthropic, *Building effective agents* —
  <https://www.anthropic.com/engineering/building-effective-agents>
- Anthropic, *Effective context engineering for AI agents* —
  <https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents>
- Anthropic tool-use tips —
  <https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview>
- OpenAI function-calling guide (<20 visible functions, merge fixed
  sequences, strict schemas) —
  <https://developers.openai.com/api/docs/guides/function-calling>
- Qwen function-calling docs (harness-side validation expected,
  description/example guidance) —
  <https://qwen.readthedocs.io/en/latest/framework/function_call.html>

Skills prior art:

- Anthropic, *Equipping agents for the real world with Agent Skills* —
  <https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills>
- Claude Code skills mechanics —
  <https://code.claude.com/docs/en/skills>
- Open skill spec — <https://agentskills.io/specification>
- MCP prompts primitive —
  <https://modelcontextprotocol.io/specification/2025-06-18/server/prompts>

Benchmarks and papers (inline arXiv IDs above):

- Berkeley Function Calling Leaderboard —
  <https://gorilla.cs.berkeley.edu/leaderboard.html>
- LongFuncEval (tool-count/context degradation) — arXiv 2505.10570
- RAG-MCP (prompt bloat, retrieval shortlisting) — arXiv 2505.03275
- Adaptive tool-list depth — arXiv 2605.24660
- Hammer (name over-reliance, function masking) — arXiv 2410.04587
- TinyLLM (per-size BFCL breakdowns, multi-turn cliff) — arXiv 2511.22138
- Qwen3 Technical Report (BFCL v3 scores) — arXiv 2505.09388
- Constraint tax (structured output vs tool invocation) — arXiv
  2606.25605, 2605.26128
