//! One conversation: human <-> stage agents over the mock backend.
//!
//! Routing rules: stage 1 starts on the first message; every later stage starts
//! on the human's next message once the backend marks the previous stage done
//! (the agent has asked whether to continue), unless that message stops the
//! application. There is no on-screen UI: the human writes naturally, and the
//! three device actions (upload, check approval, exit) arrive as a trailing
//! `[[upload]]` / `[[check_approval]]` / `[[exit]]` tag that is stripped here,
//! before the message reaches the agent, the trace or Langfuse. Upload turns
//! show the human's own words + the attachments; the agent receives those words
//! followed by the ingest step's text summary.

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
    // Separate Langfuse sessions per run of this scenario so the two sides never mingle and each
    // can be reviewed and processed on its own: the gold-loan assistant (stage agents + vision
    // extraction), the simulated human (loan officer / customer), and the run's `conversation`
    // summary, which holds both sides' messages and so lives in neither.
    let lf_session = format!("{session_id}-{}", opts.run_id);
    let (lf_agent_session, lf_human_session) = (format!("{lf_session}-agent"), format!("{lf_session}-human"));
    let lf_conversation_session = format!("{lf_session}-conversation");
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
        let (human_text, action) = split_action(&human_text);
        let upper = human_text.to_uppercase();

        // Virtual clock: typing time, plus the cooling-off wait when the human polls for approval.
        let delay = if action == Some(Action::CheckApproval) {
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

        if action == Some(Action::Exit) {
            pending_exit = true;
        }
        user = UserMsg { role: sc.human_role.clone(), text: human_text.clone(), attachments: vec![], device: device_seen.clone() };
        agent_input = human_text.clone();
        if action == Some(Action::Upload) {
            match do_upload(&backend, &mut vision, stage, sc).await {
                Ok(Some(up)) => {
                    // A bare keyword (the scripted human's "UPLOAD") carries no words of its own.
                    let said = if human_text.is_empty() || upper == "UPLOAD" { None } else { Some(human_text.clone()) };
                    user = UserMsg {
                        role: sc.human_role.clone(),
                        text: said.clone().unwrap_or_else(|| "No input message provided.".into()),
                        attachments: up.attachments,
                        device: device_seen.clone(),
                    };
                    agent_input = match said {
                        Some(w) => format!("{w}\n\n{}", up.rec.summary),
                        None => up.rec.summary.clone(),
                    };
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
        // The stage is done and its agent has asked whether to continue: any reply that does not
        // stop the application moves on (the scripted human still answers "PROCEED").
        if action != Some(Action::Exit) && stage < 6 && backend.lock().stage_done(stage) {
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
            &lf_conversation_session,
            run_start,
            chat_messages(&conversation),
            json!({"outcome": outcome_s, "status": status, "turns": conversation.len(), "loan_amount": final_state.loan_amount}),
            json!({"scenario": sc.id, "seed": sc.seed, "final_stage": stage, "agent_session": lf_agent_session, "human_session": lf_human_session, "mode": match opts.mode { Mode::Scripted => "scripted", Mode::Llm => "llm" },
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

/// A device action the human takes alongside (or instead of) typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Upload,
    CheckApproval,
    Exit,
}

/// Split the human's message into the text that goes into the conversation and the device
/// action it carries: a trailing `[[upload]]` / `[[check_approval]]` / `[[exit]]` tag (LLM
/// human; removed from the text), or a whole-message keyword (scripted human).
fn split_action(raw: &str) -> (String, Option<Action>) {
    let tag = regex::Regex::new(r"(?i)\[\[\s*([a-z_ ]+?)\s*\]\]").unwrap();
    let mut action = None;
    for c in tag.captures_iter(raw) {
        action = action.or(match c[1].to_lowercase().replace(' ', "_").as_str() {
            "upload" => Some(Action::Upload),
            "check_approval" => Some(Action::CheckApproval),
            "exit" => Some(Action::Exit),
            _ => None,
        });
    }
    let text = tag.replace_all(raw, "").split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim().to_string();
    if action.is_none() {
        action = match text.to_uppercase().as_str() {
            "UPLOAD" => Some(Action::Upload),
            "CHECK APPROVAL" => Some(Action::CheckApproval),
            "EXIT" => Some(Action::Exit),
            _ => None,
        };
    }
    (text, action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_action_tags_from_the_conversation() {
        assert_eq!(split_action("here are the PAN and Aadhaar photos [[upload]]"), ("here are the PAN and Aadhaar photos".into(), Some(Action::Upload)));
        assert_eq!(split_action("please check the approval now [[Check_Approval]]"), ("please check the approval now".into(), Some(Action::CheckApproval)));
        assert_eq!(split_action("let's stop here for now [[ exit ]]"), ("let's stop here for now".into(), Some(Action::Exit)));
        assert_eq!(split_action("yes, go ahead"), ("yes, go ahead".into(), None));
        // scripted human keywords
        assert_eq!(split_action("UPLOAD"), ("UPLOAD".into(), Some(Action::Upload)));
        assert_eq!(split_action("EXIT"), ("EXIT".into(), Some(Action::Exit)));
        // stray unknown tags never reach the conversation
        assert_eq!(split_action("ok [[click]]"), ("ok".into(), None));
    }
}
