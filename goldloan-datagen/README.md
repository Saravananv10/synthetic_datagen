# goldloan-datagen

Synthetic **gold-loan workflow** data: a *human simulator* talks to six *stage agents* that call
tools against a deterministic *mock backend*. Every conversation is written as a trace, checked by a
verifier, and can be exported as tool-calling SFT samples.

Reverse-engineered from the Pennant × Soket demo (recording + transcript). Everything the demo showed
is reproduced exactly (same tools, same order, same numbers); everything it did not show is marked
`assumed` in [`spec/business_rules.json`](spec/business_rules.json).

```
scenario = customer ground truth + scripted deviations + persona (GPT-written or rule-based)
        │
        ▼
 Human sim ◄──── chat ────► Orchestrator ──► Stage agents 1..6 ──► tools ──► Mock backend
 (GPT + persona,           stage routing,       (one harness per     (29)      OTP · UIDAI · NSDL ·
  or scripted)             virtual clock,        stage; own prompt             dedupe · valuation ·
        │                  exit / timeout        + own tools)                  schemes · bureau · GDR ·
        │ "UPLOAD"               ▲                                             e-sign · disbursal · email
        ▼                        │ ingest summary text
  scan text of the document ──► GPT as OCR/vision ──► JSON ──► ingest middleware (validates, stores what was EXTRACTED)
        │
        ▼
   trace.json ──► verifier ──► entries.jsonl / frozen snapshot ──► SFT export
```

## Quickstart

```bash
cargo build -p loop-goldloan-gen                      # simulator
cargo test  -p loop-goldloan-gen                      # 10 tests pinning the demo's numbers
cd goldloan-datagen && python3 -m pytest tests -q     # 16 tests (verifier, generator, export)

# replay the demo (deterministic, no LLM)
../target/debug/loop-goldloan-gen --scenario scenarios/golden_demo.json --out /tmp/t --force

# 300 scenarios: generate -> simulate (4 workers) -> verify -> freeze
python3 -m goldloan_gen.run_batch --n 300 --seed 2 --mode scripted --workers 4 --freeze
python3 -m goldloan_gen.export_sft --traces ../artifacts/goldloan_dataset/traces \
        --reports ../artifacts/goldloan_dataset/reports --out ../artifacts/goldloan_dataset/sft.jsonl \
        --vision-out ../artifacts/goldloan_dataset/vision_sft.jsonl   # + one sample per uploaded doc (scan text -> extracted JSON)

# everything on GPT: stage agents, human persona, OCR/vision extractor, persona writer
python3 -m goldloan_gen.run_batch --n 40 --mode llm --llm-personas --workers 4
#   --scripted-human isolates agent quality from human-sim quality; --vision deterministic skips the GPT OCR step

# same, with every model call exported to Langfuse
python3 -m goldloan_gen.run_batch --n 40 --mode llm --llm-personas --workers 4 --langfuse
```

## Credentials

`goldloan-datagen/.env` (gitignored, copy `.env.example` to start one) holds everything scoped to
**this project alone** — nothing here is read by any other tool in the workspace:

| Var | What it's for |
|---|---|
| `GOLDLOAN_SOKET_API_KEY` | The Soket/GPT key `loop-goldloan-gen` uses, for every role (agents, human, vision) and for `persona_llm.py`. Loaded explicitly (`dotenvy::from_filename`, not the generic `dotenv()` every other binary in this workspace calls) and force-set as `SOKET_API_KEY` for that process only — `loop-ai`'s `env_api_key_auth` checks that env var before the shared `~/.loop/agent/auth.json` credential, so this overrides it without ever reading or writing that shared file. Falls back to `headless-harness/.env`'s `OPENAI_API_KEY` on the Python side (`persona_llm.py`) if unset. |
| `LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY` / `LANGFUSE_BASE_URL` | Trace export via `--langfuse` (below). |

## Langfuse trace export

Pass `--langfuse` to `loop-goldloan-gen` (or `run_batch.py`) to export every model call — each stage
agent's turn, the human-sim's turn, each document's vision extraction — as a Langfuse **generation**,
grouped under one Langfuse **trace** per scenario run (same id as the trace JSON's own `session_id`,
so a Langfuse trace and its `traces/<id>.json` file always correspond 1:1). Sent via Langfuse's native
ingestion API (`POST {base_url}/api/public/ingestion`, HTTP Basic auth), not a full OTEL SDK.

Every send is fire-and-forget: spawned, logged as a `WARN` on failure, never propagated — Langfuse
being slow or down never affects the simulator or fails a run. `main.rs` waits 2s after the last
scenario before exiting so in-flight sends land before the process quits.

## Modes

| mode | agent | human | OCR / vision | use it for |
|---|---|---|---|---|
| `scripted` | deterministic oracle (`agent_scripted.rs`) | deterministic policy | true fields | golden traces, backend/verifier regression, expected behaviour |
| `llm --scripted-human` | GPT per stage, real prompts + tools | deterministic policy | GPT | measuring / generating agent behaviour |
| `llm` (default models: all `soket/gpt`) | GPT per stage | GPT role-playing a persona card | GPT | realistic human variety (typos, style, hesitation) and realistic extraction noise |

**OCR / vision = GPT.** No document images are used. For each upload the simulator renders the document's raw *scan text*
(`docs.rs`: labels, Devanagari boilerplate, payslip tables, a "photo description" for the scale), GPT returns the fields as JSON
(`vision.rs`), and a deterministic *middleware* (`Backend::ingest_*_from`) validates them (formats, name matches, weight vs
certificate, consecutive payslips…), stores what was *extracted* — not the hidden truth — and writes the `[DOCUMENT SUMMARY]` /
`[GOLD INGEST SUMMARY]` / `[PAYSLIP SUMMARY]` text the stage agent receives. Each trace turn keeps `ingest.docs[]` with
`scan_text`, `truth` and `extracted`, so extraction errors are measurable; the verifier fails a trace if a wrong extraction
was *accepted*, and only warns when validation caught it.

## Layout

| path | what |
|---|---|
| `fixtures/extract_from_demo.py`, `fixtures/demo/` | **Phase 0**: the demo's uploaded documents, cropped from the recording (PAN, Aadhaar front/back, necklace on scale, valuation certificate, payslips Apr/May/Jun). Screen-capture thumbnails upscaled 3×: fixtures/placeholders, not OCR-grade scans. |
| `spec/business_rules.json` | LTV, gold rate, schemes, thresholds — each tagged observed / assumed |
| `spec/tools.json` (`tools/build_tools_json.py`) | 29 tool schemas, tagged `observed` / `partial` / `inferred` |
| `spec/stages.json`, `spec/prompts/*.md` | stage table, six stage-agent prompts (+ common conventions), human-sim prompt |
| `scenarios/golden_demo.json` | the demo, as a scenario |
| `goldloan_gen/scenarios.py` | coverage-driven generator (happy path, 22 single deviations, 13 terminal outcomes, random combos) |
| `goldloan_gen/verify.py` | trace verifier |
| `goldloan_gen/persona_llm.py` | GPT writes each scenario's persona (name, region, occupation, employer, temperament, background, voice); output is validated, invalid fields fall back to the rule-based values |
| `goldloan_gen/run_batch.py`, `export_sft.py` | batch driver; SFT export (stage-agent tool-calling samples, and `--vision-out` for one scan-text→JSON sample per uploaded document, mirroring `vision.rs`'s exact prompt) |
| `../crates/loop-goldloan-gen/` | simulator (Rust, reuses `AgentHarness` from `loop-agent`) |

## What the demo pinned down exactly

- **Valuation**: fine weight = net × karat/22, × ₹6,000 per gram → item value; max loan = LTV × value.
  (91.48 g @ 20K → 83.1636 g → ₹498,981.82 → ₹399,185.46.) The demo showed a flat 80%; that is the slab a ₹3.99 lakh loan falls in
  under the RBI gold-loan directions effective 1 Apr 2026 (**85% up to ₹2.5 lakh, 80% up to ₹5 lakh, 75% above**), so the
  simulator uses the tiers ([Muthoot](https://www.muthootfinance.com/blog/how-much-loan-can-you-get-on-gold-rbi-rules),
  [ClearTax](https://cleartax.in/s/rbi-new-gold-loan-rules), [EY](https://www.ey.com/en_in/insights/strategy-transactions/rbi-gold-loan-guidelines-2025-impact-assessment-and-key-changes)).
  The same directions explain other demo details: no mandatory credit appraisal under ₹2.5 lakh (→ income capture only above
  ₹250,000), bullet tenure capped at 12 months, a KFS shown up front, PAN required for larger disbursals.
- **Offer**: EMI is standard reducing-balance at ROI/12 (₹35,280.67); APR = (total interest + 1% processing fee) ÷ principal ÷ years = 7.058%.
- **Gates** in `calculate_offer(action="confirm")`: `INCOME_REQUIRED` (loan > ₹250,000) → `BUREAU_REQUIRED` → confirm.
- **Behaviours**: 6-digit OTPs, `XXXXXX3210` / `BDAXX132XA` / `XXXX-XXXX-3628` masking, 60 s cooling-off before approval,
  GDR/TXN ids are epoch seconds, sanction-email OAuth timeout then retry, stage-5 "Proceed, Proceed, Proceed" pacing.
- **Absent** in the demo (kept off): no FOIR check — EMI was 72% of net pay and still passed.

## Trace format (`goldloan-trace/1`)

`conversation[].{turn, stage, clock_s, user{role,text,attachments}, agents[]{agent_name, input, tool_calls[]{name,arguments,output}, final_output{text, structured_response{response,options}, process_completed}, tokens, recorded_duration_ms, raw_text?}}`
plus `tool_definitions`/`stage_prompts` per stage, the full `scenario`, `final_state`, `outcome`.
Upload turns render as "No input message provided." + attachments in `user`, while `agents[0].input` is the ingest summary text — as in the demo.
`timing_is_simulated` is true for scripted runs (durations are synthetic).

## Verifier checks

stage order & PROCEED hand-offs · tool belongs to its stage · arguments validate against the schema · no premature calls ·
income/bureau rules before confirm · closing-chain order · valuation/LTV/EMI/APR recomputed from rules ·
**every `Rs <amount>` the agent says traces to a tool result** · no full Aadhaar/PAN/mobile/email/account in agent text ·
welcome text on stage entry · buttons allowed for the stage · glued sentences / leaked meta-text ·
outcome matches what the scenario's ground truth implies · scripted deviations actually happened (warn) ·
the human never skips a document it's supposed to have (`HUMAN_SKIPPED_UPLOAD`) · a step is never claimed before its tool succeeded
(`CLAIM_WITHOUT_TOOL`) · vision/OCR extraction checked per document against the hidden truth (wrong-but-*accepted* extractions are an
error; wrong-and-*rejected* ones are only a warning, since catching them is exactly what the re-upload path is for).

**Validated against a live `--mode llm` run with every role on `soket/gpt`** (stage agents, human persona, vision/OCR): a happy-path,
an `ocr_noise_first`, and an `extra_ornament` scenario, full 6 stages each. This surfaced three real bugs, now fixed:
1. GPT occasionally emits a garbled, truncated, or glued-sentence reply — `agent_llm.rs` now detects this in-turn (`looks_garbled`,
   `strip_glued_tail`) and re-prompts for a clean one (kept as `raw_text` if anything was changed; the verifier still flags what
   slips through).
2. GPT sometimes *announces* a step ("OTP sent successfully...") without having called the tool that performs it — `Backend::unbacked_claims`
   catches this against the tool-call log and the agent is re-prompted to actually call it; the verifier's `CLAIM_WITHOUT_TOOL` catches
   any that still slip through.
3. The human-sim persona never stated it *has* the requested documents ready, so it sometimes improvised "I don't have it" for a payslip
   upload and a `happy`-tagged scenario exited instead of completing — `persona_card()` now lists exactly what's in hand, and the UPLOAD
   rule in `human_sim.md` takes priority over the "say you don't have it" fallback.

## Known limits (be honest about them)

- **Prompts and tool descriptions are reconstructed**, not the production ones (the demo's tool descriptions were empty strings and
  the real ones are not public). The stage prompts encode the demo's observed replies; public sources were used only to sanity-check
  the business rules, not to invent prompt text.
- **Fixtures are placeholders.** Agents only see text ingest summaries. Generated scenarios reuse the demo's sample images as
  attachment placeholders (names/numbers in the images do not match the scenario). No forged-ID image generation.
- **OCR is text-in, JSON-out**: GPT reads a rendered text layer, not pixels, so it never sees skew, blur or stamps. It stands in for the
  production vision pipeline; `ocr_noise_first` injects misreads to exercise the re-upload path.
- **Not reproduced**: the production OCR / vision-LLM extraction itself (only its output format), real UIDAI/NSDL/bureau/e-sign/payment
  integrations, the client's real business rules, self-employed underwriting (routed to branch).
- **Two tool shapes are partly inferred**: `capture_income` (its name comes from another tool's error text; args guessed) and
  `allocate_and_disburse` (the demo showed `{"action": "allocate"}`; the mode/account arguments are ours).
- LLM agents occasionally glitch (duplicated sentence, leaked reasoning). Exact repeats are repaired (`raw_text` kept);
  anything else fails verification and is dropped. Per-turn rejection sampling (fork/rollback) is not implemented.
- The Soket gateway returns bursts of 502s; the agent retries for ~90 s per call before failing the run.
- Parallel workers are separate processes, each with its own session DB directory (`--keep-sessions` keeps the raw model messages).
