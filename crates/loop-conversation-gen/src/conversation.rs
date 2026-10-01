//! Per-persona `AgentHarness` setup and the round-robin turn loop.

use std::collections::HashMap;
use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::AgentEvent;
use loop_ai::{Models, ToolResultContent};
use parking_lot::Mutex;

use crate::model_util::{assistant_text, provider_error, resolve_model_spec};
use crate::spec::ConversationSpec;
use crate::trace::{ConversationTrace, ToolCallRecord, TurnRecord, TurnUsage};
use crate::web_search;

const AUTONOMY_NOTE: &str = "\n\nThis is a natural back-and-forth conversation — respond \
    at conversational length (a few sentences to a short paragraph), not an essay. Stay \
    fully in character. Never break character to mention you are an AI or a language model. \
    You are already in the middle of an ongoing conversation — only greet the other person in \
    your very first message, if at all. Every later message is a direct reply to what was just \
    said; never re-greet, re-introduce yourself, or restart the conversation as if it were new.";

/// Buffers tool-call events for the CURRENT turn only; drained after each
/// `.prompt()` call. `AgentHarness::subscribe` has no unsubscribe, so this is
/// wired once per persona harness at construction and shared via `Arc<Mutex<_>>`.
type PendingCalls = Arc<Mutex<HashMap<String, String>>>; // tool_call_id -> query
type CompletedCalls = Arc<Mutex<Vec<ToolCallRecord>>>;

struct PersonaRuntime {
    name: String,
    harness: AgentHarness,
    completed_calls: CompletedCalls,
}

pub async fn run_conversation(
    spec: &ConversationSpec,
    models: Arc<Models>,
    state_dir: &std::path::Path,
) -> anyhow::Result<ConversationTrace> {
    let mut trace = ConversationTrace::new(spec);

    let sessions_db = state_dir.join("conversations.db");
    let store = create_sqlite_session_store(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let session_repo = create_session_repository(store, None);

    // No tool here touches the filesystem, but AgentHarnessOptions still
    // requires a host_env — point it at a scratch temp dir.
    let scratch = std::env::temp_dir().join(format!("loop-conversation-gen-{}", spec.id));
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

    let mut last_line = spec.topic.clone();
    let mut last_speaker: Option<String> = None;

    'rounds: for round in 1..=spec.rounds_per_persona {
        for persona in &personas {
            let incoming = match &last_speaker {
                None => last_line.clone(),
                Some(speaker) => format!("[{speaker}]: {last_line}"),
            };

            match prompt_with_retry(&persona.harness, incoming, &persona.name).await {
                Ok(message) => finish_turn(&mut trace, persona, round, message),
                Err(err) => {
                    trace.finish_error(format!("{}: {err}", persona.name));
                    break 'rounds;
                }
            }

            let last_turn = trace.transcript.last().expect("just recorded");
            last_line = last_turn.text.clone();
            last_speaker = Some(persona.name.clone());
        }
    }

    if trace.status == "running" {
        trace.finish_ok();
    }
    Ok(trace)
}

const MAX_ATTEMPTS: u32 = 5;

/// Send `input` on the first attempt, then a short retry notice on
/// subsequent attempts (so the model isn't confused by its own missing
/// turn), with exponential backoff between attempts (1s, 2s, 4s, 8s). The
/// Soket API has shown a meaningfully high transient-502/504 rate under
/// sustained multi-turn load this session — one retry wasn't enough in
/// practice.
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
            incoming_retry_notice()
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

fn incoming_retry_notice() -> String {
    "(The previous attempt hit a transient error — please respond to the conversation so far.)"
        .to_string()
}

fn finish_turn(
    trace: &mut ConversationTrace,
    persona: &PersonaRuntime,
    round: u32,
    message: loop_agent::AgentMessage,
) {
    let text = assistant_text(&message);
    let (input, output) = usage_of(&message);
    let tool_calls = std::mem::take(&mut *persona.completed_calls.lock());
    trace.record_turn(TurnRecord {
        round,
        persona: persona.name.clone(),
        text,
        tool_calls,
        usage: TurnUsage { input, output },
    });
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
            completed.lock().push(ToolCallRecord {
                tool: tool_name,
                query,
                result_preview: preview,
            });
        }
        _ => {}
    }
}
