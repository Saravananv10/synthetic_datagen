//! The N-way group-conversation turn loop. Not related to `conversation.rs`'s
//! `run_conversation` — that pipeline is untouched; this is a new, separate
//! engine. The one new mechanic versus the 1-on-1 pipeline: a shared
//! broadcast log with a per-persona "what have I heard" cursor, since with
//! N>2 personas a speaker may have missed *multiple* lines since their last
//! turn (the 2-persona case never needed this — there was always exactly one
//! pending line by construction).

use std::collections::HashMap;
use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::AgentEvent;
use loop_ai::{Models, ToolResultContent};
use parking_lot::Mutex;
use rand::Rng;

use crate::group_spec::{GroupConversationSpec, TurnOrder};
use crate::group_trace::{
    GroupConversationTrace, GroupToolCallRecord, GroupTurnRecord, GroupTurnUsage,
};
use crate::model_util::{assistant_text, provider_error, resolve_model_spec};
use crate::web_search;

const AUTONOMY_NOTE: &str = "\n\nThis is a live group conversation with several participants — \
    respond at conversational length (a few sentences to a short paragraph), not an essay. \
    Stay fully in character. Never break character to mention you are an AI or a language \
    model. Respond to specific points made since you last spoke, not just the most recent \
    line — several people may have spoken since your last turn. You will see other \
    participants' lines prefixed like \"[Name]: text\" — that's just how their turns are \
    labeled for you. Do not prefix your own reply with your own name in brackets; that's \
    added automatically. Just write your response directly. You are already in the middle of \
    an ongoing conversation — only greet the group in your very first message, if at all; never \
    re-greet or re-introduce yourself in later turns.";

type PendingCalls = Arc<Mutex<HashMap<String, String>>>; // tool_call_id -> query
type CompletedCalls = Arc<Mutex<Vec<GroupToolCallRecord>>>;

struct PersonaRuntime {
    name: String,
    harness: AgentHarness,
    completed_calls: CompletedCalls,
}

struct BroadcastEntry {
    speaker: String,
    text: String,
}

pub async fn run_group_conversation(
    spec: &GroupConversationSpec,
    models: Arc<Models>,
    state_dir: &std::path::Path,
) -> anyhow::Result<GroupConversationTrace> {
    let mut trace = GroupConversationTrace::new(spec);

    let sessions_db = state_dir.join("group-conversations.db");
    let store = create_sqlite_session_store(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let session_repo = create_session_repository(store, None);

    let scratch = std::env::temp_dir().join(format!("loop-group-conversation-gen-{}", spec.id));
    std::fs::create_dir_all(&scratch)?;
    let host_env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&scratch));

    let mut personas = Vec::with_capacity(spec.personas.len());
    for persona_spec in &spec.personas {
        let model = resolve_model_spec(&models, &persona_spec.model)
            .ok_or_else(|| anyhow::anyhow!("unknown model: {}", persona_spec.model))?;

        let tools = if persona_spec.allow_web_search {
            vec![web_search::build_tool()]
        } else {
            vec![]
        };

        let session = session_repo
            .create(None, Some(format!("persona:{}", persona_spec.name)))
            .await
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?;

        let harness = AgentHarness::new(AgentHarnessOptions {
            models: Arc::clone(&models),
            model: Some(model),
            session,
            host_env: Arc::clone(&host_env),
            tools,
            system_prompt: format!("{}{AUTONOMY_NOTE}", persona_spec.system_prompt),
            sandbox: SandboxMode::Disabled,
            resources: AgentHarnessResources {
                skills: Vec::new(),
                prompt_templates: Vec::new(),
            },
        });

        let pending: PendingCalls = Arc::new(Mutex::new(HashMap::new()));
        let completed: CompletedCalls = Arc::new(Mutex::new(Vec::new()));
        {
            let pending = Arc::clone(&pending);
            let completed = Arc::clone(&completed);
            harness.subscribe(move |event| {
                let pending = Arc::clone(&pending);
                let completed = Arc::clone(&completed);
                async move { handle_event(event, pending, completed) }
            });
        }

        personas.push(PersonaRuntime {
            name: persona_spec.name.clone(),
            harness,
            completed_calls: completed,
        });
    }

    let mut log: Vec<BroadcastEntry> = Vec::new();
    let mut heard: HashMap<String, usize> = personas.iter().map(|p| (p.name.clone(), 0)).collect();

    let total_turns = spec.rounds_per_persona * spec.personas.len() as u32;
    let mut speaker_idx = 0usize;
    let mut prev_speaker: Option<usize> = None;

    for turn_no in 1..=total_turns {
        let i = match spec.turn_order {
            TurnOrder::RoundRobin => speaker_idx,
            TurnOrder::Random => pick_random_speaker(personas.len(), prev_speaker),
        };
        let persona = &personas[i];

        let incoming = if log.is_empty() {
            format!(
                "{}\n\n(You are opening this discussion — no one has spoken yet. Do not refer \
                 to things other participants have supposedly already said.)",
                spec.topic
            )
        } else {
            let cursor = *heard.get(&persona.name).unwrap_or(&0);
            log[cursor..]
                .iter()
                .map(|e| format!("[{}]: {}", e.speaker, e.text))
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        match prompt_with_retry(&persona.harness, incoming, &persona.name).await {
            Ok(message) => {
                let mut text = assistant_text(&message);
                let (mut input, mut output) = usage_of(&message);
                if text.trim().is_empty() {
                    // The model finished via tool calls alone and closed with an
                    // empty message — observed live in this pipeline (Marcus/gpt,
                    // twice in one run) and in loop-datagen-server earlier this
                    // session. Force one explicit follow-up rather than recording
                    // a silent blank turn.
                    tracing::warn!(
                        "{}: empty turn text, requesting an explicit follow-up",
                        persona.name
                    );
                    let followup = prompt_with_retry(
                        &persona.harness,
                        "You finished without saying anything. Reply now with your actual \
                         response to the discussion so far, in plain text."
                            .to_string(),
                        &persona.name,
                    )
                    .await;
                    if let Ok(followup_msg) = followup {
                        let followup_text = assistant_text(&followup_msg);
                        if !followup_text.trim().is_empty() {
                            text = followup_text;
                            let (fi, fo) = usage_of(&followup_msg);
                            input += fi;
                            output += fo;
                        }
                    }
                }
                let tool_calls = std::mem::take(&mut *persona.completed_calls.lock());
                trace.record_turn(GroupTurnRecord {
                    turn: turn_no,
                    persona: persona.name.clone(),
                    text: text.clone(),
                    tool_calls,
                    usage: GroupTurnUsage { input, output },
                });

                log.push(BroadcastEntry {
                    speaker: persona.name.clone(),
                    text,
                });
                heard.insert(persona.name.clone(), log.len());
                speaker_idx = (speaker_idx + 1) % personas.len();
                prev_speaker = Some(i);
            }
            Err(err) => {
                trace.finish_error(format!("{}: {err}", persona.name));
                break;
            }
        }
    }

    if trace.status == "running" {
        trace.finish_ok();
    }
    Ok(trace)
}

fn pick_random_speaker(n: usize, prev: Option<usize>) -> usize {
    if n <= 1 {
        return 0;
    }
    let mut rng = rand::thread_rng();
    loop {
        let candidate = rng.gen_range(0..n);
        if Some(candidate) != prev {
            return candidate;
        }
    }
}

const MAX_ATTEMPTS: u32 = 5;

/// Copied from `conversation.rs`'s `prompt_with_retry` rather than shared —
/// this file is meant to be fully self-contained per the isolation goal for
/// this pipeline. If the retry strategy is tuned later, both copies would
/// need updating; a conscious, documented trade-off, not an oversight.
async fn prompt_with_retry(
    harness: &AgentHarness,
    input: String,
    persona_name: &str,
) -> Result<loop_agent::AgentMessage, String> {
    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        let this_input = if attempt == 1 {
            input.clone()
        } else {
            "(The previous attempt hit a transient error — please respond to the conversation \
             so far.)"
                .to_string()
        };
        match harness.prompt(this_input).await {
            Ok(message) => match provider_error(&message) {
                None => return Ok(message),
                Some(err) => {
                    tracing::warn!(
                        "{persona_name}: provider error (attempt {attempt}/{MAX_ATTEMPTS}): {err}"
                    );
                    last_err = err;
                }
            },
            Err(e) => {
                tracing::warn!(
                    "{persona_name}: harness error (attempt {attempt}/{MAX_ATTEMPTS}): {e}"
                );
                last_err = e.to_string();
            }
        }
        if attempt < MAX_ATTEMPTS {
            let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
            tokio::time::sleep(backoff).await;
        }
    }
    Err(format!("provider error after {MAX_ATTEMPTS} attempts: {last_err}"))
}

fn usage_of(message: &loop_agent::AgentMessage) -> (u64, u64) {
    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = message {
        (am.usage.input, am.usage.output)
    } else {
        (0, 0)
    }
}

fn handle_event(event: AgentEvent, pending: PendingCalls, completed: CompletedCalls) {
    match event {
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } if tool_name == "web_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            pending.lock().insert(tool_call_id, query);
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            ..
        } if tool_name == "web_search" => {
            let query = pending.lock().remove(&tool_call_id).unwrap_or_default();
            let preview = result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
                .chars()
                .take(400)
                .collect::<String>();
            completed.lock().push(GroupToolCallRecord {
                tool: tool_name,
                query,
                result_preview: preview,
            });
        }
        _ => {}
    }
}
