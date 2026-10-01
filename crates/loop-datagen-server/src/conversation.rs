//! One `Conversation` = one coordinator `AgentHarness`, long-lived across
//! turns (each turn is a fresh gRPC stream; continuity is the harness
//! staying resident in the [`crate::registry::ConversationRegistry`] keyed
//! by session id).

use std::sync::Arc;

use loop_agent::harness::AgentHarness;
use loop_agent::{AgentEvent, AssistantMessageEvent, Model};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tonic::Status;

use crate::approval::GrpcApprovalBridge;
use crate::pb;

type OutTx = mpsc::Sender<Result<pb::ServerMessage, Status>>;

pub struct Conversation {
    pub harness: Arc<AgentHarness>,
    /// Client-facing session id (kept for diagnostics / future tracing).
    #[allow(dead_code)]
    pub session_id: String,
    current_out: Arc<Mutex<Option<OutTx>>>,
    approval: Arc<Mutex<Option<Arc<GrpcApprovalBridge>>>>,
}

impl Conversation {
    /// Wrap a freshly built coordinator harness. Wires the one-time
    /// `subscribe()` translator — `AgentHarness::subscribe` has no
    /// unsubscribe, so this must be called exactly once per harness
    /// instance, forwarding to whichever turn currently owns `current_out`.
    pub fn new(session_id: String, harness: Arc<AgentHarness>) -> Arc<Self> {
        let current_out: Arc<Mutex<Option<OutTx>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&current_out);
        harness.subscribe(move |event| {
            let sink = Arc::clone(&sink);
            async move {
                if let Some(msg) = translate_event(event) {
                    let out = sink.lock().clone();
                    if let Some(out) = out {
                        let _ = out.send(Ok(msg)).await;
                    }
                }
            }
        });

        Arc::new(Self {
            harness,
            session_id,
            current_out,
            approval: Arc::new(Mutex::new(None)),
        })
    }

    /// Run one turn: stream events to `out`, honoring approval/cancel on the
    /// same stream, and send a terminal `FinalResponse`/`ErrorResponse`.
    pub async fn run_turn(
        &self,
        message: String,
        model_override: Option<Model>,
        out: OutTx,
        auto_approve: bool,
    ) {
        *self.current_out.lock() = Some(out.clone());
        let bridge = Arc::new(GrpcApprovalBridge::new(out.clone(), auto_approve));
        *self.approval.lock() = Some(Arc::clone(&bridge));
        self.harness
            .set_before_tool_call(Some(bridge.before_tool_hook()));

        if let Some(model) = model_override {
            self.harness.set_model(model).await;
        }

        let result = self.harness.prompt(message).await;

        *self.current_out.lock() = None;
        *self.approval.lock() = None;
        self.harness.set_before_tool_call(None);

        match result {
            Ok(msg) => {
                // The underlying model/provider call can fail *inside* a
                // nominally `Ok` AgentMessage (loop-ai reports provider
                // errors via `stop_reason`/`error_message` on the assistant
                // message rather than an `Err` here) — surface that as a
                // real ErrorResponse instead of silently returning empty
                // text with zero usage, which is indistinguishable from a
                // genuinely quiet turn.
                if let Some(err) = provider_error(&msg) {
                    tracing::warn!("coordinator turn ended in provider error: {err}");
                    let _ = out
                        .send(Ok(pb::ServerMessage {
                            event: Some(pb::server_message::Event::Error(pb::ErrorResponse {
                                message: err,
                                code: "PROVIDER_ERROR".into(),
                            })),
                        }))
                        .await;
                    return;
                }
                let (full_text, prompt_tokens, completion_tokens) = final_text_and_usage(&msg);
                let _ = out
                    .send(Ok(pb::ServerMessage {
                        event: Some(pb::server_message::Event::Done(pb::FinalResponse {
                            full_text,
                            prompt_tokens,
                            completion_tokens,
                        })),
                    }))
                    .await;
            }
            Err(e) => {
                let _ = out
                    .send(Ok(pb::ServerMessage {
                        event: Some(pb::server_message::Event::Error(pb::ErrorResponse {
                            message: e.to_string(),
                            code: "AGENT_ERROR".into(),
                        })),
                    }))
                    .await;
            }
        }
    }

    /// Resolve a pending `ActionRequired` from a client `UserInput`.
    pub fn resolve_user_input(&self, prompt_id: &str, reply: String) {
        if let Some(bridge) = self.approval.lock().clone() {
            bridge.resolve(prompt_id, reply);
        }
    }

    /// Cancel the in-flight turn, if any.
    pub fn cancel(&self) {
        self.harness.abort();
    }
}

/// If the turn's final message is a provider/runtime error (empty content,
/// `stop_reason: Error`), return a human-readable message for it.
pub(crate) fn provider_error(message: &loop_agent::AgentMessage) -> Option<String> {
    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = message {
        if matches!(am.stop_reason, loop_ai::StopReason::Error | loop_ai::StopReason::Aborted) {
            return Some(
                am.error_message
                    .clone()
                    .unwrap_or_else(|| format!("model call ended with stop_reason {:?}", am.stop_reason)),
            );
        }
    }
    None
}

/// Extract the final assistant text plus (prompt_tokens, completion_tokens)
/// from the message `AgentHarness::prompt()` returns.
fn final_text_and_usage(message: &loop_agent::AgentMessage) -> (String, i32, i32) {
    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = message {
        let text = am
            .content
            .iter()
            .filter_map(|c| match c {
                loop_ai::AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        (
            text,
            am.usage.input as i32,
            am.usage.output as i32,
        )
    } else {
        (String::new(), 0, 0)
    }
}

fn translate_event(event: AgentEvent) -> Option<pb::ServerMessage> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => Some(pb::ServerMessage {
            event: Some(pb::server_message::Event::TextChunk(pb::TextChunk {
                text: delta,
            })),
        }),
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(pb::ServerMessage {
            event: Some(pb::server_message::Event::ToolStart(pb::ToolCallStart {
                tool_name,
                arguments_json: args.to_string(),
                tool_use_id: tool_call_id,
            })),
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            let output = result
                .content
                .iter()
                .filter_map(|c| match c {
                    loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(pb::ServerMessage {
                event: Some(pb::server_message::Event::ToolResult(pb::ToolCallResult {
                    tool_name,
                    output,
                    is_error,
                    tool_use_id: tool_call_id,
                })),
            })
        }
        _ => None,
    }
}
