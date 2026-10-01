//! One conversation: human <-> stage agents over the mock backend.
//!
//! Routing rules (from the demo): stage 1 starts on the first message; every
//! later stage starts when the human sends PROCEED after the previous stage's
//! completion message. Uploads appear in the chat as "No input message
//! provided." + attachments, while the agent receives the ingest step's text
//! summary.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use loop_ai::Models;
use parking_lot::Mutex;
use serde_json::json;

use crate::agent_llm::LlmAgents;
use crate::agent_scripted::ScriptedAgent;
use crate::backend::Backend;
use crate::human::{Human, HumanView, LlmHuman, ScriptedHuman};
use crate::langfuse::Langfuse;
use crate::reply::{parse_reply, AgentOut};
use crate::rules::Rules;
use crate::scenario::Scenario;
use crate::stages;
use crate::tools;
use crate::trace::{AgentRec, DocRec, FinalOutput, IngestRec, Tokens, ToolCallRec, Trace, Turn, UserMsg};
use crate::vision::{LlmVision, Vision};
use crate::docs;

/// A healthy stage takes at most ~15 turns (a few deviations included); beyond this the agent is stuck.
const MAX_TURNS_PER_STAGE: u32 = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Scripted,
    Llm,
}

pub struct RunOpts {
    pub mode: Mode,
    pub max_turns: u32,
    pub agent_model: String,
    pub human_model: String,
    /// Use the deterministic scripted human even in `Llm` mode (isolates agent quality).
    pub scripted_human: bool,
    /// GPT plays the OCR/vision extractor (otherwise the true fields are used as-is).
    pub vision_llm: bool,
    pub vision_model: String,
    /// Distinguishes this invocation's Langfuse traces from earlier runs of the same scenarios.
    pub run_id: String,
}

enum AgentDriver {
    Scripted(ScriptedAgent),
    Llm(Box<LlmAgents>),
}

pub async fn run(
    sc: &Scenario,
    rules: &Rules,
    opts: &RunOpts,
    models: Option<Arc<Models>>,
    state_dir: &Path,
    lf: Langfuse,
) -> anyhow::Result<Trace> {
    let session_id = format!("{}-{}", sc.id, sc.seed);
    // Two Langfuse sessions per run of this scenario, one per side, so each can be reviewed and
    // processed on its own: the application side (stage agents + vision extraction + the run's
    // `conversation` summary) and the simulated human.
    let lf_session = format!("{session_id}-{}", opts.run_id);
    let (lf_agent_session, lf_human_session) = (format!("{lf_session}-agent"), format!("{lf_session}-human"));
    let run_start = chrono::Utc::now();
    let backend = Arc::new(Mutex::new(Backend::new(rules.clone(), sc.clone())));
    let mut agent = match opts.mode {
        Mode::Scripted => AgentDriver::Scripted(ScriptedAgent::default()),
        Mode::Llm => {
            let m = models.clone().ok_or_else(|| anyhow::anyhow!("llm mode needs models"))?;
            AgentDriver::Llm(Box::new(LlmAgents::new(m, &opts.agent_model, Arc::clone(&backend), state_dir, &sc.id, lf.clone(), lf_agent_session.clone())?))
        }
    };
    let mut human = if opts.mode == Mode::Llm && !opts.scripted_human {
        let m = models.clone().ok_or_else(|| anyhow::anyhow!("llm mode needs models"))?;
        Human::Llm(LlmHuman::new(sc, m, &opts.human_model, state_dir, lf.clone(), lf_human_session.clone()).await?)
    } else {
        Human::Scripted(ScriptedHuman::new(sc))
    };

    let mut vision = if opts.vision_llm {
        let m = models.clone().ok_or_else(|| anyhow::anyhow!("llm vision needs models"))?;
        Vision::Llm(LlmVision::new(m, &opts.vision_model, state_dir, &sc.id, lf.clone(), lf_agent_session.clone())?)
    } else {
        Vision::Deterministic
    };
    let mut next_ingest: Option<IngestRec> = None;

    let greetings = ["Hey", "Hi", "Hello", "hi"];
    let greeting = greetings[(sc.seed % greetings.len() as u64) as usize].to_string();
    let mut user = UserMsg { role: sc.human_role.clone(), text: greeting.clone(), attachments: vec![], device: vec![] };
    let mut agent_input = greeting;

    let mut conversation: Vec<Turn> = vec![];
    let mut stage: u8 = 1;
    let mut turn_in_stage = 0u32;
    let mut turn = 0u32;
    let mut pending_exit = false;
    let mut early_check_done = false;
    let mut status = "completed".to_string();
    let mut outcome: Option<String> = None;

    loop {
        turn += 1;
        turn_in_stage += 1;
        backend.lock().stage = stage;

        // ---- agent turn ----
        let started = Instant::now();
        let out: AgentOut = match &mut agent {
            AgentDriver::Scripted(a) => {
                let raw = a.respond(stage, &agent_input, &mut backend.lock());
                let (text, options) = parse_reply(&raw);
                let n_calls = backend.lock().log.len() as u64;
                AgentOut { text, raw_text: None, options, input_tokens: 0, output_tokens: 0, duration_ms: 250 + 180 * n_calls + 4 * text_len(&raw) }
            }
            AgentDriver::Llm(a) => match a.respond(stage, &agent_input).await {
                Ok(o) => o,
                Err(e) => {
                    status = format!("error: {e}");
                    outcome = Some("error".into());
                    break;
                }
            },
        };
        let _ = started;
        let (calls, clock, terminal, items) = {
            let mut be = backend.lock();
            let calls: Vec<ToolCallRec> = be.log.drain(..).map(Into::into).collect();
            (calls, be.s.clock_s, be.s.outcome.clone(), be.s.gold_items.len())
        };
        let completed_now = terminal.as_deref() == Some("completed");
        conversation.push(Turn {
            turn,
            stage,
            clock_s: clock,
            user: user.clone(),
            ingest: next_ingest.take(),
            agents: vec![AgentRec {
                agent_name: stages::stage(stage).agent_label,
                input: agent_input.clone(),
                tool_calls: calls,
                final_output: FinalOutput {
                    text: out.text.clone(),
                    structured_response: json!({"response": out.text, "options": out.options}),
                    process_completed: completed_now,
                },
                tokens: Tokens { input_tokens: out.input_tokens, output_tokens: out.output_tokens, total_tokens: out.input_tokens + out.output_tokens },
                recorded_duration_ms: out.duration_ms,
                raw_text: out.raw_text.clone(),
            }],
        });

        // ---- termination ----
        if terminal.is_some() {
            outcome = terminal;
            break;
        }
        if pending_exit {
            outcome = Some("exited".into());
            break;
        }
        if turn_in_stage >= MAX_TURNS_PER_STAGE {
            outcome = Some("stalled".into());
            status = format!("error: stalled in stage {stage} for {turn_in_stage} turns");
            break;
        }
        if turn >= opts.max_turns {
            outcome = Some("timeout".into());
            status = "error: max_turns".into();
            break;
        }

        // ---- human turn ----
        let device = std::mem::take(&mut backend.lock().inbox);
        let device_seen = device.clone();
        let view = HumanView { stage, turn_in_stage, agent_text: &out.text, options: out.options.as_deref(), device, items_collected: items };
        let human_text = match human.reply(&view).await {
            Ok(t) => t,
            Err(e) => {
                status = format!("error: {e}");
                outcome = Some("error".into());
                break;
            }
        };
        let upper = human_text.trim().to_uppercase();

        // Virtual clock: typing time, plus the cooling-off wait when the human polls for approval.
        let delay = if upper == "CHECK APPROVAL" {
            if sc.flags.check_approval_early && !early_check_done {
                early_check_done = true;
                8
            } else {
                rules.closing.cooling_off_seconds + 2
            }
        } else {
            5 + (turn as u64 * 7) % 16
        };
        backend.lock().advance(delay);

        if upper == "EXIT" {
            pending_exit = true;
        }
        user = UserMsg { role: sc.human_role.clone(), text: human_text.trim().to_string(), attachments: vec![], device: device_seen.clone() };
        agent_input = human_text.trim().to_string();
        if upper == "UPLOAD" {
            match do_upload(&backend, &mut vision, stage, sc).await {
                Ok(Some(up)) => {
                    user = UserMsg { role: sc.human_role.clone(), text: "No input message provided.".into(), attachments: up.attachments, device: device_seen.clone() };
                    agent_input = up.rec.summary.clone();
                    next_ingest = Some(up.rec);
                }
                Ok(None) => {}
                Err(e) => {
                    status = format!("error: {e}");
                    outcome = Some("error".into());
                    break;
                }
            }
        }
        if upper == "PROCEED" && stage < 6 && backend.lock().stage_done(stage) {
            stage += 1;
            turn_in_stage = 0;
        }
    }

    let (final_state, outcome_s) = {
        let be = backend.lock();
        (be.s.clone(), outcome.or_else(|| be.s.outcome.clone()).unwrap_or_else(|| "unknown".into()))
    };
    if let Some(c) = &lf {
        c.conversation(
            &lf_agent_session,
            run_start,
            chat_messages(&conversation),
            json!({"outcome": outcome_s, "status": status, "turns": conversation.len(), "loan_amount": final_state.loan_amount}),
            json!({"scenario": sc.id, "seed": sc.seed, "final_stage": stage, "human_session": lf_human_session, "mode": match opts.mode { Mode::Scripted => "scripted", Mode::Llm => "llm" },
                   "agent_model": opts.agent_model, "human_model": opts.human_model, "vision_model": opts.vision_model, "customer": sc.customer.name}),
            sc.tags.clone(),
        );
    }
    let stage_ids: Vec<u8> = (1..=6).collect();
    Ok(Trace {
        schema: "goldloan-trace/1",
        session_id,
        scenario_id: sc.id.clone(),
        seed: sc.seed,
        mode: match opts.mode { Mode::Scripted => "scripted".into(), Mode::Llm => "llm".into() },
        models: match opts.mode {
            Mode::Scripted => json!({"agent": "scripted-oracle", "human": "scripted-policy"}),
            Mode::Llm => json!({"agent": opts.agent_model, "human": if opts.scripted_human { "scripted-policy".to_string() } else { opts.human_model.clone() }}),
        },
        timing_is_simulated: opts.mode == Mode::Scripted,
        tool_definitions: json!(stage_ids.iter().map(|s| (s.to_string(), tools::definitions_for_stage(*s))).collect::<std::collections::BTreeMap<_, _>>()),
        stage_prompts: json!(stage_ids.iter().map(|s| (s.to_string(), stages::system_prompt(*s))).collect::<std::collections::BTreeMap<_, _>>()),
        conversation,
        status,
        outcome: outcome_s,
        final_state: Some(final_state),
        scenario: serde_json::to_value(sc)?,
    })
}

fn text_len(s: &str) -> u64 {
    s.chars().count() as u64
}

struct Upload {
    attachments: Vec<String>,
    rec: IngestRec,
}

/// The human said UPLOAD: decide which documents that means right now, run them
/// through the OCR/vision step, then the ingest middleware. `None` = nothing to upload.
async fn do_upload(be: &Arc<Mutex<Backend>>, vision: &mut Vision, stage: u8, sc: &Scenario) -> anyhow::Result<Option<Upload>> {
    let (kind, docs): (&str, Vec<docs::Doc>) = {
        let mut b = be.lock();
        match stage {
            2 if !b.s.identity_stored => {
                let a = b.attempt("identity");
                ("identity", docs::identity_docs(sc, a))
            }
            3 if sc.flags.override_address && b.s.address.is_none() => ("address_proof", docs::address_proof_docs(sc)),
            4 => {
                let idx = b.s.gold_items.len();
                let a = b.attempt("gold");
                match docs::gold_docs(sc, idx, a) {
                    Some(d) => ("gold", d),
                    None => return Ok(None),
                }
            }
            5 if b.s.employment_type.as_deref() == Some("salaried") && !b.s.payslips_ok => {
                let a = b.attempt("payslips");
                let good = sc.income.payslips.clone();
                let set = if sc.flags.payslips_bad_first && a == 0 {
                    if sc.income.payslips_bad.is_empty() { good[..good.len().saturating_sub(1)].to_vec() } else { sc.income.payslips_bad.clone() }
                } else {
                    good
                };
                ("payslips", docs::payslip_docs(sc, &set))
            }
            _ => return Ok(None),
        }
    };

    let (mut tin, mut tout) = (0u64, 0u64);
    let mut recs: Vec<DocRec> = vec![];
    for d in &docs {
        let scan = docs::render(d, sc);
        let out = vision.extract(d, &scan).await?;
        tin += out.input_tokens;
        tout += out.output_tokens;
        recs.push(DocRec { doc: d.kind.to_string(), fixture: d.fixture.clone(), scan_text: scan, truth: d.fields.clone(), extracted: out.fields });
    }
    let by = |k: &str| recs.iter().find(|r| r.doc == k).map(|r| r.extracted.clone());
    let summary = {
        let mut b = be.lock();
        match kind {
            "identity" => {
                let (front, back) = (by("aadhaar_front").unwrap_or_default(), by("aadhaar_back").unwrap_or_default());
                b.ingest_identity_from(&front, &back, by("pan").as_ref())
            }
            "address_proof" => b.ingest_address_proof_from(&by("address_proof").unwrap_or_default()),
            "gold" => b.ingest_gold_from(&by("gold_photo").unwrap_or_default(), &by("gold_certificate").unwrap_or_default()),
            _ => {
                let slips: Vec<serde_json::Value> = recs.iter().map(|r| r.extracted.clone()).collect();
                b.ingest_payslips_from(&slips)
            }
        }
    };
    Ok(Some(Upload {
        attachments: recs.iter().map(|r| r.fixture.clone()).filter(|f| !f.is_empty()).collect(),
        rec: IngestRec { kind: kind.into(), vision_model: vision.label(), docs: recs, summary, input_tokens: tin, output_tokens: tout },
    }))
}

/// The run as a Langfuse chat (`[{role, content}]`): each turn's human message (plus any
/// uploads) as `user`, then each agent reply as `assistant`.
fn chat_messages(conversation: &[Turn]) -> serde_json::Value {
    let mut msgs = vec![];
    for t in conversation {
        let mut user = t.user.text.clone();
        if !t.user.attachments.is_empty() {
            user.push_str(&format!("\n[attached: {}]", t.user.attachments.join(", ")));
        }
        msgs.push(json!({"role": "user", "content": user}));
        for a in &t.agents {
            msgs.push(json!({"role": "assistant", "name": a.agent_name, "content": a.final_output.text}));
        }
    }
    serde_json::Value::Array(msgs)
}
