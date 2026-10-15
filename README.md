# Synthetic Datagen using Loop

Synthetic conversation and agent-trace datasets, generated with **[Loop](#built-on-loop)**, Soket AI's Rust agent harness.

Every generator follows the same idea: several LLM agents, each running in its own Loop `AgentHarness`, talk to each other (and, where relevant, call tools). Each run is saved as a trace JSON, and batches are collected into a dataset with a manifest.

| Pipeline | What it produces | Engine (Rust) | Batch layer (Python) |
|---|---|---|---|
| **Gold-loan workflow** | A simulated customer/loan officer talks to 6 stage agents that call 29 tools against a mock bank backend. Traces are verified and exported as tool-calling SFT data | [`crates/loop-goldloan-gen`](crates/loop-goldloan-gen) | [`goldloan-datagen/`](goldloan-datagen) |
| **2-person conversations** | Long 1-on-1 conversations between two personas (e.g. farmer and agronomist) | [`crates/loop-conversation-gen`](crates/loop-conversation-gen), binary `loop-conversation-gen` | [`conversation-gen/`](conversation-gen) |
| **Group conversations** | N-persona discussions and debates (2v2, 3v1, roundtable) | same crate, binary `loop-group-conversation-gen` | [`conversation-gen/`](conversation-gen) |
| **Multilingual conversations** | 42 languages × 28 domains, using the 2-person or group engine depending on the domain | both binaries above | `conversation-gen/conversation_gen/multilingual_gen.py` |

## Repository layout

```
crates/
  loop-ai, loop-agent, loop-app-core, loop-mcp   Loop core used by the generators
  loop-goldloan-gen/                             gold-loan simulator
  loop-conversation-gen/                         2-person + group conversation engines
  loop-cli, loop-desktop, ...                    the rest of Loop (not needed for datagen)
goldloan-datagen/
  spec/            stages, 29 tool schemas, business rules, agent + human-sim prompts
  scenarios/       golden_demo.json (the reference scenario)
  goldloan_gen/    scenario generator, batch runner, verifier, SFT export, GPT personas
conversation-gen/
  conversation_gen/  spec generators, batch runner, checkpoint/curation, manifest, domains, languages
artifacts/         generated specs, traces and datasets (see below)
```

## Setup

```bash
cargo build -p loop-goldloan-gen -p loop-conversation-gen
```

Credentials (all `.env` files are gitignored):

| File | Variables | Used by |
|---|---|---|
| `goldloan-datagen/.env` (copy `.env.example`) | `GOLDLOAN_SOKET_API_KEY`, `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY`, `LANGFUSE_BASE_URL` | gold-loan simulator and Langfuse export |
| `conversation-gen/.env` | `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL` | gold-loan GPT persona writer (`persona_llm.py`) |
| `.env` at the repo root | `TAVILY_API_KEY` | `web_search` tool in the conversation engines |

The conversation engines use the Soket provider configured for Loop (`~/.loop/agent/auth.json`, or `SOKET_API_KEY`).

## Gold-loan workflow

```bash
cd goldloan-datagen

# replay the reference demo (deterministic, no LLM)
../target/debug/loop-goldloan-gen --scenario scenarios/golden_demo.json --out /tmp/t --force

# generate -> simulate -> verify -> freeze, scripted agents
python3 -m goldloan_gen.run_batch --n 300 --seed 2 --mode scripted --workers 4 --freeze

# everything on GPT (stage agents, human, OCR step, personas), traced to Langfuse
python3 -m goldloan_gen.run_batch --n 40 --mode llm --llm-personas --workers 4 --langfuse

# export tool-calling SFT samples
python3 -m goldloan_gen.export_sft --traces ../artifacts/goldloan_dataset/traces \
  --reports ../artifacts/goldloan_dataset/reports --out ../artifacts/goldloan_dataset/sft.jsonl
```

Details (stages, tools, business rules, verifier checks, Langfuse layout): [`goldloan-datagen/README.md`](goldloan-datagen/README.md).

## 2-person conversations

```bash
# build the spec bank (persona archetypes × topic seeds, no model calls)
cd conversation-gen
python3 -m conversation_gen.spec_gen

# run it: skips specs already done, writes traces + manifest
python3 -m conversation_gen.cli --dry-run      # list what would run
python3 -m conversation_gen.cli --limit 5      # quick sample
python3 -m conversation_gen.cli --freeze-dataset
```

A spec gives the opening `topic`, `rounds_per_persona` and two `personas` (`name`, `model`, `system_prompt`, `allow_web_search`). Specs live in `artifacts/conversation_task_bank/`.

## Group conversations

```bash
# run from the repo root (output paths are relative to it)
./target/debug/loop-group-conversation-gen --spec artifacts/group_conversation_task_bank/nuclear_energy_2v2.json
```

Group specs add `turn_order` (`round_robin` or `random`) and any number of personas. Teams such as 2v2 or 3v1 are set up in each persona's prompt. Example specs: `nuclear_energy_2v2`, `remote_work_2v2`, `id_verification_3v1`, `learning_to_code_roundtable`.

## Multilingual conversations

```bash
cd conversation-gen
python3 -m conversation_gen.multilingual_gen --count 100 --languages hindi,tamil --domains banking
```

This writes one JSONL shard per (domain, language) to `artifacts/multilingual_task_bank/`. Both engines read a shard directly with `--spec <file>.jsonl`. The full bank (42 languages × 28 domains × 10,000) is about 29 GB, so it is **not committed**; regenerate it with the command above.

## Outputs

| Folder | Contents |
|---|---|
| `artifacts/goldloan_dataset/` | gold-loan scenarios, traces, verifier reports, `entries.jsonl`, snapshots |
| `artifacts/goldloan_dataset_live/` | gold-loan runs with GPT on every role |
| `artifacts/conversation_task_bank/`, `conversation_dataset/`, `conversation_checkpoint/` | 2-person specs, traces, manifest and run state |
| `artifacts/group_conversation_task_bank/`, `group_conversation_dataset/` | group specs and traces |
| `artifacts/multilingual_conversation_dataset/` | multilingual traces |

## Built on Loop

Loop is a production-grade AI harness in Rust: a unified LLM API, a stateful agent and an interactive coding CLI. The generators use these crates:

| Crate | Role |
|---|---|
| [`loop-ai`](crates/loop-ai) | Unified LLM API, Soket provider, OpenAI-compatible providers |
| [`loop-agent`](crates/loop-agent) | Agent loop, `AgentHarness`, tools, sessions, sandbox, skills |
| [`loop-app-core`](crates/loop-app-core) | Shared bootstrap, config and runtime wiring |
| [`loop-mcp`](crates/loop-mcp) | MCP client and server |

The Loop CLI is also here: `cargo run -p loop-cli` (see [`crates/loop-cli/README.md`](crates/loop-cli/README.md)). To build and test the workspace:

```bash
cargo build
cargo test -p loop-ai -p loop-agent -p loop-goldloan-gen
```
