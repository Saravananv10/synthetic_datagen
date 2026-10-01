# How the Loop harness works, and how the new conversation-generator uses it

This explains two things, simply:

1. **The default Loop harness** — the engine that already powers `loop-cli` and `loop-desktop` (i.e. the thing a human chats with).
2. **The new conversation-generator** (`loop-conversation-gen` + `headless-harness/conversation_gen`) — a new tool that reuses that *same* engine to make two AI personas talk to each other automatically, thousands of times over, to produce a synthetic dataset.

The key idea to hold onto: **the conversation-generator does not reimplement the agent.** It just calls the same `AgentHarness` that the CLI calls, once per persona, and wires the personas' outputs into each other instead of into a human's keyboard.

---

## Part 1 — The default Loop harness

### The cast of characters

```
loop-ai       →  "the model"      talks to an LLM provider (Soket, OpenAI-compatible, etc.)
loop-agent    →  "the brain"      runs the send→stream→tool-call→repeat loop
loop-cli /
loop-desktop  →  "the face"       shows the conversation to a human, collects input
```

Everything below lives in `crates/loop-agent`.

### `AgentHarness` — the object you actually talk to

Think of `AgentHarness` (`crates/loop-agent/src/harness/agent_harness.rs`) as **one chat window with memory**. You create one with:

- which **model** to use (`loop_ai::Model`)
- a **system prompt** (the persona/instructions)
- a **session** (where the conversation history is stored — JSONL file or SQLite)
- a list of **tools** it's allowed to call (read file, write file, run bash, web search, ...)
- a **sandbox mode** (run tools directly on the host, or inside a container/microVM)

Once built, you drive it with one method:

```rust
harness.prompt("some text").await  // -> AgentMessage (the assistant's reply)
```

Every call to `.prompt(...)` automatically:
- appends your message to the session's history,
- sends the **whole conversation so far** to the model,
- runs any tool calls the model asks for,
- feeds tool results back to the model and lets it keep going until it produces a final answer,
- saves everything to the session store,
- returns the final assistant message.

So `AgentHarness` already "remembers" — you never have to replay the transcript yourself.

### What happens inside `.prompt()` — the actual loop

`.prompt()` calls into `agent_loop.rs`'s `run_loop()` (`crates/loop-agent/src/agent_loop.rs:159`). In plain English, one turn looks like this:

```
┌─────────────────────────────────────────────────────────────┐
│ 1. Send system prompt + full message history to the model   │
│ 2. Stream the model's reply back (text, and/or tool calls)  │
│ 3. Did it ask to call a tool (read/write/bash/web_search)?  │
│      yes → run the tool(s), append the tool's result to the │
│            history, go back to step 1                       │
│      no  → this is the final answer, stop                   │
└─────────────────────────────────────────────────────────────┘
```

This is exactly the "ReAct"-style loop most coding agents use: *think → act (tool call) → observe (tool result) → think again*, repeated until the model stops calling tools.

A few extra pieces worth knowing about, because the conversation-generator leans on them:

- **Events**: every step above emits an `AgentEvent` (`ToolExecutionStart`, `ToolExecutionEnd`, `MessageEnd`, `TurnEnd`, ...). You can `harness.subscribe(|event| { ... })` once, and get a live feed of everything the agent does — this is how tool calls get logged without touching the core loop.
- **Sessions**: the conversation history isn't just in memory — it's persisted (`harness/session/`), so a harness can be killed and resumed, or forked into branches.
- **Sandboxing**: tools like `bash` don't have to run on the bare host; `harness/sandbox/` can route them through a container or microVM.
- **Provider errors look like normal messages**: if the underlying LLM call fails (e.g. a 502 from the API), `loop-ai` doesn't always return a Rust `Err` — it can return an `Ok` message with empty text and `stop_reason: Error`. Code that drives the harness has to check for this explicitly, or it'll silently treat a failed call as "the model said nothing."

### How the CLI uses it

`loop-cli` reads user keystrokes → calls `harness.prompt(text)` → renders the streamed events to the terminal. That's the entire "face" — all the actual intelligence is in the harness described above.

---

## Part 2 — The new conversation-generator

### The problem it solves

The team wants a large **synthetic dataset** of realistic multi-turn advisory conversations — e.g. "a farmer asks a question, an agronomy expert answers, they go back and forth for a dozen rounds" — to use as training/eval data. Hand-writing these would be far too slow, so this is an automated pipeline that:

1. Describes a conversation as a small JSON file (a **spec**).
2. Spins up one `AgentHarness` per persona (reusing Part 1's engine — nothing new is built for "the agent" itself).
3. Makes the personas take turns talking to *each other* instead of a human, for many rounds.
4. Records the full transcript + every tool call as a trace file.
5. Repeats this for hundreds of specs, resumably, and rolls the results into a dataset manifest.

It has two layers: a **Rust binary** that runs one conversation, and a **Python batch layer** that generates lots of specs and drives the Rust binary over all of them.

### 2a. The spec — what one conversation looks like

`crates/loop-conversation-gen/src/spec.rs` defines the input file:

```json
{
  "id": "farmer_005_...",
  "topic": "My soil test came back showing low nitrogen and slightly acidic pH...",
  "rounds_per_persona": 8,
  "personas": [
    { "name": "Farmer",           "model": "soket/qwen3-8-27b", "system_prompt": "...", "allow_web_search": false },
    { "name": "Agronomy Expert",  "model": "soket/gpt",         "system_prompt": "...", "allow_web_search": true  }
  ]
}
```

- `topic` is the literal opening line handed to persona #1 — not an abstract label.
- `rounds_per_persona` × number of personas = total turns (8 rounds × 2 personas = 16 turns).
- Each persona can have a **different model** and a **different system prompt** — that's what gives the asker and the expert distinct "voices."
- `allow_web_search` opts a persona into the `web_search` tool (typically the expert, so answers are grounded in real info instead of guesses).

You can point the binary at one spec file, or a whole directory of them (it just loads every `*.json` in it).

### 2b. Building one `AgentHarness` per persona

`crates/loop-conversation-gen/src/conversation.rs` — for each persona in the spec, it builds a harness exactly the way Part 1 describes:

```rust
AgentHarness::new(AgentHarnessOptions {
    models,
    model: resolve_model_spec(&models, &persona.model),   // "soket/gpt-oss-120b" -> Model
    session: session_repo.create(...).await,                // its own private session
    host_env,                                                 // no filesystem tools needed here
    tools: if persona.allow_web_search { vec![web_search::build_tool()] } else { vec![] },
    system_prompt: format!("{}{AUTONOMY_NOTE}", persona.system_prompt),
    sandbox: SandboxMode::Disabled,
    ...
})
```

`AUTONOMY_NOTE` is a fixed suffix appended to every persona's system prompt, nudging it to reply at natural conversational length and never break character.

Each harness also gets `harness.subscribe(...)` wired up once, so `ToolExecutionStart`/`ToolExecutionEnd` events for `web_search` get captured into a small per-persona buffer — that's how the trace ends up with a record of exactly what was searched and what came back, without touching the core agent loop at all.

Because `AgentHarness` already keeps its own session history, the generator only ever needs to hand it **the other persona's latest line** — it does not need to replay the whole transcript manually.

### 2c. The round-robin turn loop

This is the actual "conversation":

```
last_line = spec.topic          # persona[0] gets the raw question first
last_speaker = None

for round in 1..=rounds_per_persona:
    for persona in personas:                       # speaking order = list order
        incoming = last_line                         if first ever turn
                 = "[<last_speaker>]: <last_line>"    otherwise
        reply = persona.harness.prompt(incoming)      # <- Part 1's engine does the work
        record_turn(persona, round, reply, tool_calls_captured)
        last_line = reply.text
        last_speaker = persona.name
```

So persona A and persona B are just two independent `AgentHarness` chat sessions, and the generator's whole job is to **pipe A's output into B's input, and B's output into A's input**, tagging each line with who said it so the other side doesn't get confused about who's talking.

**Retry handling**: `prompt_with_retry()` (`conversation.rs`) wraps each `.prompt()` call. Because the team hit real transient 502/504 errors from the model API under sustained multi-turn load, every call is retried up to 5 times with exponential backoff (1s, 2s, 4s, 8s), checking specifically for the "`Ok` message but `stop_reason: Error`" case described in Part 1 — a plain Rust `Err` alone isn't enough to catch these. If all 5 attempts fail, the whole conversation is marked `"status": "error"` and stops early rather than silently producing garbage.

### 2d. The `web_search` tool

`crates/loop-conversation-gen/src/web_search.rs` — a normal `AgentTool` (same shape as the CLI's `read`/`write`/`bash` tools), backed by the **Tavily** search API (`TAVILY_API_KEY` env var required). It's given to a persona exactly like any other tool — the model decides on its own when to call it (e.g. the expert persona calling it mid-answer to check current best practices, prices, or product names) — there is no special-casing for it inside the core loop.

> Note: an earlier design considered scraping DuckDuckGo's HTML with no API key, but that gets CAPTCHA-blocked for automated traffic in practice, so the shipped version uses Tavily instead.

### 2e. The trace — the actual dataset row

`crates/loop-conversation-gen/src/trace.rs` writes one JSON file per conversation to `artifacts/conversation_dataset/traces/<id>.json`:

```json
{
  "conversation_id": "farmer_005_...",
  "topic": "...",
  "personas": [ { "name": "Farmer", "model": "soket/qwen3-8-27b", "allow_web_search": false }, ... ],
  "transcript": [
    { "round": 1, "persona": "Farmer", "text": "...", "tool_calls": [], "usage": {"input": 186, "output": 2061} },
    { "round": 1, "persona": "Agronomy Expert", "text": "...",
      "tool_calls": [ { "tool": "web_search", "query": "...", "result_preview": "..." } ],
      "usage": {"input": 1395, "output": 1299} }
  ],
  "status": "completed",
  "usage_totals": { "Farmer": {...}, "Agronomy Expert": {...} }
}
```

This is a real example already in the repo — see [`artifacts/conversation_dataset/traces/farmer_005_...json`](artifacts/conversation_dataset/traces/farmer_005_my-soil-test-came-back-showing-low-nitrogen-and-slightly-aci.json): the Farmer asks about liming and nitrogen, the Agronomy Expert calls `web_search` to ground its liming-rate numbers, and the two keep going for several rounds, each building on the last.

### 2f. Scaling up: generating hundreds of specs (Python)

Hand-writing specs doesn't scale, so `headless-harness/conversation_gen/spec_gen.py` **combinatorially** generates them:

- A list of **persona archetypes** (Farmer, Student, Small Business Owner, New Homeowner, New Pet Owner, Job Seeker, Home Cook, ...), each with a system-prompt template and a handful of realistic **topic seeds**.
- Every archetype is always paired with a fixed "responder" persona for that domain (e.g. Farmer ↔ Agronomy Expert), asker model = `soket/qwen3-8-27b`, responder model = `soket/gpt`, responder gets `allow_web_search: true`.
- It just cartesian-combines archetype × topic seed → one spec file per combination, written into `artifacts/conversation_task_bank/`. A handful of archetypes × a handful of topics each already produces dozens of specs with zero extra model calls.

### 2g. The batch runner (Python)

`headless-harness/conversation_gen/cli.py` is the thing you actually run to generate "a lot" of dataset:

```
for each spec in artifacts/conversation_task_bank/*.json:
    if already done (checkpoint) or marked good (curation) and not --force-rerun:
        skip
    else:
        run:  ./target/debug/loop-conversation-gen --spec <file> --out <traces dir>
        read back the trace JSON it wrote
        append one line to artifacts/conversation_dataset/entries.jsonl (the manifest)
        mark the spec done or failed in the checkpoint store
```

It reuses two pieces already built for the (separate, older) `datagen` pipeline:
- **`CheckpointStore`** — remembers which spec IDs are already done/failed, so an interrupted batch run of hundreds of specs can be safely re-launched and will just pick up where it left off instead of redoing everything.
- **`CurationList`** — lets you mark specs as "good, skip forever" independent of the checkpoint.

`entries.jsonl` is an **append-only manifest** (one JSON line per conversation: id, a hash of the topic, `"Farmer(qwen3-8-27b) vs Agronomy Expert(gpt)"`, round count, turn count, status). Running `--freeze-dataset` hashes the whole current manifest into a content-addressed **snapshot** file under `artifacts/conversation_dataset/snapshots/<dataset_id>.json` — an immutable "this is dataset version X" cut you can point training/eval code at.

---

## Part 3 — How it all fits together, end to end

```
headless-harness/conversation_gen/spec_gen.py
        │  writes many *.json spec files
        ▼
artifacts/conversation_task_bank/*.json
        │
        │  headless-harness/conversation_gen/cli.py loops over these,
        │  skipping ones already done (checkpoint/curation), and for
        │  each undone spec shells out to:
        ▼
crates/loop-conversation-gen  (Rust binary)
        │  loads the spec, then for EACH persona:
        │    builds one AgentHarness  ──────────────►  Part 1's engine
        │    (same code path loop-cli uses for a human)   (crates/loop-agent)
        │
        │  round-robins persona.prompt(other's last line)
        │  for rounds_per_persona × personas turns,
        │  capturing text + web_search tool calls via
        │  harness.subscribe(...)
        ▼
artifacts/conversation_dataset/traces/<id>.json   (one full transcript)
        │
        │  cli.py reads this back and appends a summary row
        ▼
artifacts/conversation_dataset/entries.jsonl      (manifest, append-only)
        │
        │  --freeze-dataset hashes the manifest
        ▼
artifacts/conversation_dataset/snapshots/<hash>.json   (immutable dataset cut)
```

**The one sentence version**: the CLI drives one `AgentHarness` with a human on the other end; the conversation-generator drives *two or more* `AgentHarness` instances and points them at each other instead, then wraps that in a Python layer that can generate hundreds of scenarios and grind through them resumably, unattended, into a dataset.

---

## Where things live (quick reference)

| What | Where |
|---|---|
| Core agent loop (send → stream → tool call → repeat) | `crates/loop-agent/src/agent_loop.rs` |
| The stateful "one chat session" object | `crates/loop-agent/src/harness/agent_harness.rs` |
| Built-in tools (read/write/edit/bash) | `crates/loop-agent/src/harness/tools/mod.rs` |
| Conversation spec format | `crates/loop-conversation-gen/src/spec.rs` |
| Per-persona harness setup + round-robin loop | `crates/loop-conversation-gen/src/conversation.rs` |
| `web_search` tool (Tavily-backed) | `crates/loop-conversation-gen/src/web_search.rs` |
| Trace/output JSON format | `crates/loop-conversation-gen/src/trace.rs` |
| CLI entry point for the Rust binary | `crates/loop-conversation-gen/src/main.rs` |
| Combinatorial spec generator (persona × topic) | `headless-harness/conversation_gen/spec_gen.py` |
| Batch runner (checkpoint/curation/manifest) | `headless-harness/conversation_gen/cli.py` |
| Manifest + dataset-freeze logic | `headless-harness/conversation_gen/manifest.py` |
| Generated specs (input) | `artifacts/conversation_task_bank/` |
| Generated conversations (output) | `artifacts/conversation_dataset/traces/` |
| Original design write-up | `happy-launching-flame.md` |
