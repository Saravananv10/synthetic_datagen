# Multi-persona advisory-conversation synthetic dataset generator

## Context

Inspired loosely by *"Courtroom-Style Multi-Agent Debate with Progressive RAG and
Role-Switching for Controversial Claim Verification"* (PROClaim, arXiv:2603.28488) —
specifically its idea of agents grounding a multi-turn exchange in real retrieved
evidence round by round. Everything else about the paper (adversarial debate
framing, judge panel, verdicts, role-switching, static FAISS/PubMed corpus) has been
explicitly dropped per direction.

**What this actually is**, per the concrete example given: a persona with a real
need (e.g. "a farmer asks for advice about a pest problem") talks with a
responder (an expert/assistant persona, possibly a different underlying model,
possibly grounded via live web search) — the initial question naturally unfolds
into a multi-turn conversation as the asker follows up, and the responder digs
deeper. The goal is to run **many** such conversations, across many
persona/topic combinations, and capture each full conversation + every tool call
as a trace — a synthetic multi-turn advisory-dialogue dataset.

- **Search backend: DuckDuckGo**, no API key — chosen for a quick start, so nothing
  blocks getting the first real dataset out. (Documented as swappable later — Tavily
  or another provider would need a small tool-implementation swap, not an
  architecture change, if DuckDuckGo's HTML scraping proves too fragile.)
- **No judge, no verdict, no role-switching, no multi-judge panel.** The
  conversation transcript itself is the dataset row.
- Personas are **not** inherently adversarial — "Farmer asks, Expert answers, both
  keep talking" is the primary shape; nothing in the architecture assumes debate
  specifically (an asker and a responder are just two personas with different
  system prompts, same as any other pair).

Reuses this session's `loop-datagen-server` lessons: `AgentHarness` (not bare
`loop-ai`) is required specifically because of tool calling — `AgentHarness`'s own
agentic loop already handles "call `web_search`, feed results back, continue,"
which a bare `Models::complete_simple()` call does not. Also reused: the
provider-error-surfacing pattern (`stop_reason`/`error_message` check before
trusting empty text — this session found that exact bug twice against the Soket
API) and the `provider/model_id` model-spec convention from
`crates/loop-datagen-server/src/subagents.rs`'s `resolve_model_spec`.

---

## Architecture

```
crates/loop-conversation-gen/         (NEW Rust binary crate)
  src/main.rs          — CLI: run one ConversationSpec file, or every *.json in a
                          directory (batch mode — this is the primary use case,
                          "a lot of synthetic dataset")
  src/spec.rs           — ConversationSpec / PersonaSpec (serde, load from JSON)
  src/web_search.rs      — DuckDuckGo-backed `web_search` AgentTool
  src/conversation.rs     — per-persona AgentHarness setup + round-robin turn loop
                            + trace capture (text + tool-call events)
  src/trace.rs              — trace/manifest JSON writing

artifacts/conversation_task_bank/*.json   (NEW — ConversationSpec files, one per
                                            conversation to generate; see "Scaling
                                            up" below for how these get generated
                                            in bulk rather than hand-written)
artifacts/conversation_dataset/
  entries.jsonl                            (append-only manifest)
  traces/<conversation_id>.json            (full transcript + tool-call trace)
  snapshots/<dataset_id>.json              (freeze output)

headless-harness/conversation_gen/    (Python batch layer — this is where bulk spec
                                        generation + resumable batch running live,
                                        not optional given "a lot of dataset")
  spec_gen.py    — generates many ConversationSpec files from persona/topic seeds
  cli.py         — runs a directory of specs, checkpoint/curation-aware
  manifest.py
  (reuses datagen/checkpoint.py's CheckpointStore and datagen/curation.py's
   CurationList verbatim — both are generic over task_id: str)
```

---

## Part 1 — `ConversationSpec` (`crates/loop-conversation-gen/src/spec.rs`)

```rust
#[derive(Deserialize)]
struct ConversationSpec {
    id: String,
    topic: String,                  // the opening line — e.g. the farmer's actual
                                      // question, not an abstract "topic label"
    rounds_per_persona: u32,         // "longer conversations" — suggested default: 8-12
                                      // (2 personas → 16-24 total turns)
    personas: Vec<PersonaSpec>,      // 2+; order = speaking order, round-robin.
                                      // First persona's system_prompt should frame
                                      // them as the one with the question; later
                                      // personas as responder(s)/expert(s).
}

#[derive(Deserialize)]
struct PersonaSpec {
    name: String,                   // "Farmer", "Agronomy Expert", "Assistant", ...
    model: String,                  // "soket/gpt-oss-120b" — provider/id convention,
                                      // same as resolve_model_spec
    system_prompt: String,
    allow_web_search: bool,          // per-persona opt-in to the web_search tool
                                      // (typically true for the responder/expert,
                                      // optional for the asker)
}
```

Example spec (`artifacts/conversation_task_bank/farmer_pest_advice_001.json`):
```json
{
  "id": "farmer_pest_advice_001",
  "topic": "My tomato plants have curling yellow leaves and I'm seeing tiny white insects underneath. What's going on and what should I do?",
  "rounds_per_persona": 10,
  "personas": [
    {
      "name": "Farmer",
      "model": "soket/qwen3-8-27b",
      "system_prompt": "You are a small-scale vegetable farmer. You have a real, specific problem and want practical advice. Ask natural follow-up questions based on what the expert tells you — mention your specific conditions (climate, what you've already tried) as they come up, the way a real person would in conversation, not all at once up front.",
      "allow_web_search": false
    },
    {
      "name": "Agronomy Expert",
      "model": "soket/gpt",
      "system_prompt": "You are an agricultural extension expert. Give specific, actionable advice, not generic platitudes. When current information would help (current pest identification, regional treatment recommendations, product names), use web_search rather than guessing. Ask clarifying questions when the farmer's description is ambiguous.",
      "allow_web_search": true
    }
  ]
}
```

Load one file or every `*.json` in a directory.

## Part 2 — Per-persona `AgentHarness` + round-robin loop (`conversation.rs`)

Each persona gets **one long-lived `AgentHarness`** for the entire conversation (own
session, own model, own system prompt, `web_search` tool attached iff
`allow_web_search`). `AgentHarness` already accumulates its own session history
across repeated `.prompt()` calls, so feeding it just the other persona's latest
line each turn is enough — no manual transcript-replay needed.

```rust
async fn build_persona_harness(persona: &PersonaSpec, models: &Arc<Models>, session_repo: &SessionRepository) -> AgentHarness {
    let tools = if persona.allow_web_search { vec![web_search::build_tool()] } else { vec![] };
    let system_prompt = format!(
        "{}\n\n{}",
        persona.system_prompt,
        "This is a natural back-and-forth conversation — respond at conversational \
         length (a few sentences to a short paragraph), not an essay. Stay fully in \
         character.",
    );
    AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(models),
        model: resolve_model_spec(models, &persona.model).expect("unknown model"),
        session: session_repo.create(None, Some(format!("persona:{}", persona.name))).await.unwrap(),
        host_env: /* minimal no-op ExecutionEnv — no filesystem access needed */,
        tools,
        system_prompt,
        sandbox: SandboxMode::Disabled,
        resources: AgentHarnessResources { skills: vec![], prompt_templates: vec![] },
    })
}
```

**Turn loop**:
```rust
let mut harnesses: Vec<AgentHarness> = build one per persona;
let mut last_line = spec.topic.clone();   // persona[0] receives the raw opening
                                            // question as-is on round 1
let mut last_speaker: Option<&str> = None;

for round in 1..=spec.rounds_per_persona {
    for (i, persona) in spec.personas.iter().enumerate() {
        let incoming = match last_speaker {
            None => last_line.clone(),
            Some(s) => format!("[{s}]: {last_line}"),
        };
        let message = harnesses[i].prompt(incoming).await?;
        if let Some(err) = provider_error(&message) {          // reused pattern —
            record_error_turn(persona, round, err);              // this session found
            /* retry once, then abort with status: "error" */     // this exact bug twice
        }
        let text = assistant_text(&message);
        record_turn(persona, round, &text, captured_tool_calls_for(i));
        last_line = text;
        last_speaker = Some(&persona.name);
    }
}
```

**Tool-call trace capture**: `harnesses[i].subscribe(...)` once per persona harness
at construction (no unsubscribe exists, so wire once — same pattern as
`loop-datagen-server`'s `Conversation::new`), pushing
`ToolExecutionStart`/`ToolExecutionEnd` events into that persona's trace buffer,
keyed by round number.

## Part 3 — `web_search` tool, DuckDuckGo-backed (`web_search.rs`)

No API key. GET `https://html.duckduckgo.com/html/?q=<query>` with a real
browser-style `User-Agent` header (DuckDuckGo's lite HTML endpoint, no JS
required), parse result blocks out of the HTML with the `scraper` crate (new dep —
CSS-selector based HTML parsing), extract `{title, url, snippet}` per result
(DDG's HTML result markup uses stable-ish classes like `.result__title`,
`.result__snippet`, `.result__url` — verify against a live fetch during
implementation since scraping is inherently a bit fragile to markup changes).

```rust
pub fn build_tool() -> AgentTool {
    AgentTool {
        name: "web_search".into(),
        description: "Search the live web. Use this to ground advice in real, \
                       current information rather than guessing.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        execute: Arc::new(|_id, args, _cancel, _cb| Box::pin(async move {
            let query = args["query"].as_str().ok_or("missing query")?;
            let html = reqwest::Client::new()
                .get("https://html.duckduckgo.com/html/")
                .query(&[("q", query)])
                .header("User-Agent", "Mozilla/5.0 (compatible; loop-conversation-gen)")
                .send().await.map_err(|e| e.to_string())?
                .text().await.map_err(|e| e.to_string())?;
            let results = parse_duckduckgo_html(&html);   // Vec<{title, url, snippet}>, top ~5
            Ok(AgentToolResult { content: vec![ToolResultContent::Text(TextContent{ text: format_results(&results), text_signature: None })], details: json!(results), .. })
        })),
        ..
    }
}
```

New Rust deps: `scraper` (HTML parsing). `reqwest` is already a workspace
dependency. No secrets/config needed — this is the "as of now, quick start" choice;
if DuckDuckGo scraping proves unreliable in practice, the fix is swapping this one
file for a Tavily/Brave call behind the same `AgentTool` interface, not an
architecture change.

## Part 4 — Trace + manifest output (`trace.rs`)

**Per-conversation trace** → `artifacts/conversation_dataset/traces/<conversation_id>.json`:
```json
{
  "conversation_id": "...", "topic": "...", "created_at": "...",
  "personas": [{"name": "Farmer", "model": "soket/qwen3-8-27b", "allow_web_search": false}, ...],
  "transcript": [
    {
      "round": 1, "persona": "Farmer", "text": "...",
      "tool_calls": [],
      "usage": {"input": N, "output": N}
    },
    {
      "round": 1, "persona": "Agronomy Expert", "text": "...",
      "tool_calls": [{"tool": "web_search", "query": "tomato leaf curl whitefly treatment", "result_preview": "..."}],
      "usage": {"input": N, "output": N}
    }
  ],
  "status": "completed" | "error",
  "usage_totals": {"Farmer": {"input": N, "output": N}, "Agronomy Expert": {...}}
}
```

**Manifest entry** (Python, `conversation_gen/manifest.py`, JSONL-appended to
`artifacts/conversation_dataset/entries.jsonl`, reusing
`dataset_manifest.append_entry`/`freeze_dataset` pointed at new paths):
```python
@dataclass
class ConversationManifestEntry:
    conversation_id: str
    topic_hash: str
    personas: str            # "Farmer(qwen3-8-27b) vs Agronomy Expert(gpt)"
    round_count: int
    total_turns: int
    status: str               # "completed" | "error"
    recorded_at: str
```

## Part 5 — Scaling up: generating many specs (`headless-harness/conversation_gen/spec_gen.py`)

Since the actual goal is "a lot of synthetic dataset like this," hand-writing each
`ConversationSpec` doesn't scale. v1 approach: a **combinatorial template
generator** — no extra model calls, fast, deterministic:

```python
PERSONA_ARCHETYPES = [
    {"name": "Farmer", "system_prompt_template": "...", "model": "soket/qwen3-8-27b"},
    {"name": "Student", "system_prompt_template": "...", "model": "soket/qwen3-8-27b"},
    {"name": "Small Business Owner", "system_prompt_template": "...", "model": "soket/qwen3-8-27b"},
    # ...
]
RESPONDER = {"name": "Assistant", "model": "soket/gpt", "allow_web_search": True}

TOPIC_SEEDS = {
    "Farmer": ["pest problem with tomatoes", "irrigation scheduling for a drought year",
               "when to sell grain given current prices", ...],
    "Student": ["how to structure a research paper literature review", ...],
    ...
}
```
Cartesian-combine persona × topic seed → one `ConversationSpec` per combination,
written to `artifacts/conversation_task_bank/`. This alone can produce hundreds of
specs from a modest seed list.

**v2 stretch** (only if the template approach feels too repetitive once you see
real output): an LLM-based spec generator — one cheap model call per spec asking
for a specific, naturalistic opening question for a given persona archetype,
instead of picking from a fixed seed list — trading a small amount of extra cost
for more topic diversity.

## Part 6 — Python batch layer (`headless-harness/conversation_gen/cli.py`)

Loads specs from `artifacts/conversation_task_bank/`, skips via reused
`CheckpointStore`/`CurationList` (resumable across interrupted batch runs — directly
relevant given how many specs "a lot of dataset" implies), invokes the Rust binary
per spec as a subprocess, appends manifest entries, supports `--freeze-dataset`.

---

## Milestones

1. **M1** — crate skeleton, `ConversationSpec` loading, 2-persona long round-robin
   loop **without** web search, trace output. Validate live with the farmer/pest
   example above (no search yet) — confirm the conversation stays natural and
   in-character across 8-10 rounds each, doesn't degenerate into repetition, and
   both personas' distinct models are genuinely producing distinct voices.
2. **M2** — `web_search` tool via DuckDuckGo HTML scraping, opt-in per persona.
   Validate live that the responder actually calls it mid-conversation on a
   question where real current info matters, and cites something real (not
   fabricated) in the next turn.
3. **M3** — provider-error retry handling (reuse the `stop_reason`/`error_message`
   check — real 502/504s were hit repeatedly against the Soket API this session),
   trace-capture polish.
4. **M4** — `spec_gen.py` combinatorial generator + a real persona-archetype/topic
   seed list (10-20 archetypes × 5-10 seeds each = 50-200 specs) + the Python batch
   `cli.py` running the full bank with checkpoint/manifest/freeze.
5. **Documented, not built**: judge/verdict layer, role-switching, multi-judge
   panel, LLM-based spec diversification (v2 stretch from Part 5) — all explicitly
   out of scope unless you ask for them later.

---

## Verification

- `cargo build -p loop-conversation-gen` — compiles against `loop-agent`'s public
  `AgentHarness` API, same pattern as `loop-datagen-server` (no internal patches
  needed).
- Live run: the farmer/pest-advice example end to end; inspect the trace JSON for
  (a) a coherent, non-repetitive multi-turn advisory conversation, (b) at least one
  real `web_search` call with a genuine query/result once M2 lands, (c) correct
  per-persona model attribution.
- `python -m conversation_gen.spec_gen` then `python -m conversation_gen.cli` against
  the generated bank, confirming checkpoint/manifest files write correctly and a
  batch of, say, 10 specs runs to completion unattended.
