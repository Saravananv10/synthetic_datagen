//! Rebuilds, for Langfuse, exactly what each model call inside a harness turn sent and got
//! back. One `h.prompt()` can make several model calls (tool call -> result -> next call ...);
//! a [`Recorder`] subscribed to the harness's event stream sees every message and tool run in
//! order, and [`Recorder::finish`] replays them on top of the session history the turn started
//! from, producing one [`Step`] per model call with the full OpenAI-format `messages` it was
//! given, plus one [`ToolRun`] per tool execution with real timings.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use loop_agent::harness::AgentHarness;
use loop_agent::{AgentEvent, AgentMessage, Message};
use loop_ai::{AssistantContent, ToolResultContent, UserContent, UserMessageContent};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::langfuse::{Step, ToolRun};

enum Rec {
    TurnStart,
    Msg(AgentMessage),
    ToolStart { id: String, name: String, args: Value },
    ToolEnd { id: String, result: Value, is_error: bool },
}

#[derive(Clone, Default)]
pub struct Recorder {
    events: Arc<Mutex<Vec<(DateTime<Utc>, Rec)>>>,
}

impl Recorder {
    /// Subscribe a new recorder to `h`'s events (for the harness's whole lifetime).
    pub fn attach(h: &AgentHarness) -> Self {
        let r = Recorder::default();
        let events = Arc::clone(&r.events);
        h.subscribe(move |e| {
            // Recorded synchronously, so timestamps are when the event happened.
            let rec = match e {
                AgentEvent::TurnStart => Some(Rec::TurnStart),
                AgentEvent::MessageEnd { message } => Some(Rec::Msg(message)),
                AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args } => Some(Rec::ToolStart { id: tool_call_id, name: tool_name, args }),
                AgentEvent::ToolExecutionEnd { tool_call_id, result, is_error, .. } => {
                    let result = if result.details.is_null() || result.details == json!({}) { json!(tool_result_text(&result.content)) } else { result.details };
                    Some(Rec::ToolEnd { id: tool_call_id, result, is_error })
                }
                _ => None,
            };
            if let Some(rec) = rec {
                events.lock().push((Utc::now(), rec));
            }
            async {}
        });
        r
    }

    /// Start a new capture window, returning the session history the next model call builds on.
    pub async fn begin(&self, h: &AgentHarness) -> Vec<AgentMessage> {
        self.events.lock().clear();
        h.session_context().await.map(|c| c.messages).unwrap_or_default()
    }

    /// Everything since [`Recorder::begin`], as model calls and tool runs.
    pub fn finish(&self, system: &str, history: Vec<AgentMessage>) -> (Vec<Step>, Vec<ToolRun>) {
        let events = std::mem::take(&mut *self.events.lock());
        let mut ctx: Vec<Value> = vec![json!({"role": "system", "content": system})];
        ctx.extend(history.iter().filter_map(openai_message));
        let (mut steps, mut runs) = (vec![], vec![]);
        let mut step_start = events.first().map(|(t, _)| *t).unwrap_or_else(Utc::now);
        let mut open_tools: HashMap<String, (DateTime<Utc>, String, Value)> = HashMap::new();
        for (t, rec) in events {
            match rec {
                Rec::TurnStart => step_start = t,
                Rec::Msg(m) => {
                    if let AgentMessage::Llm(Message::Assistant(am)) = &m {
                        let thinking = am
                            .content
                            .iter()
                            .filter_map(|c| if let AssistantContent::Thinking(th) = c { Some(th.thinking.as_str()) } else { None })
                            .collect::<Vec<_>>()
                            .join("\n");
                        steps.push(Step {
                            messages: Value::Array(ctx.clone()),
                            output: openai_message(&m).unwrap_or(Value::Null),
                            thinking,
                            input_tokens: am.usage.input,
                            output_tokens: am.usage.output,
                            start: step_start,
                            end: t,
                            stop_reason: format!("{:?}", am.stop_reason),
                            error: am.error_message.clone(),
                        });
                    }
                    if let Some(v) = openai_message(&m) {
                        ctx.push(v);
                    }
                }
                Rec::ToolStart { id, name, args } => {
                    open_tools.insert(id, (t, name, args));
                }
                Rec::ToolEnd { id, result, is_error } => {
                    if let Some((start, name, args)) = open_tools.remove(&id) {
                        runs.push(ToolRun { call_id: id, name, args, result, is_error, start, end: t });
                    }
                }
            }
        }
        (steps, runs)
    }
}

fn tool_result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => t.text.as_str(),
            ToolResultContent::Image(_) => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A message as it goes over the wire (mirrors `loop-ai`'s `convert_message` for this
/// provider: reasoning is NOT replayed to the model, so it is left out here too).
fn openai_message(m: &AgentMessage) -> Option<Value> {
    let AgentMessage::Llm(m) = m else { return None };
    Some(match m {
        Message::User(u) => {
            let content = match &u.content {
                UserMessageContent::Text(t) => json!(t),
                UserMessageContent::Blocks(b) => json!(b
                    .iter()
                    .map(|c| match c {
                        UserContent::Text(t) => json!({"type": "text", "text": t.text}),
                        UserContent::Image(_) => json!({"type": "image_url", "image_url": {"url": "[image]"}}),
                    })
                    .collect::<Vec<_>>()),
            };
            json!({"role": "user", "content": content})
        }
        Message::Assistant(a) => {
            let text: String = a.content.iter().filter_map(|c| if let AssistantContent::Text(t) = c { Some(t.text.as_str()) } else { None }).collect();
            let calls: Vec<Value> = a
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(json!({"id": tc.id, "type": "function", "function": {"name": tc.name, "arguments": tc.arguments.to_string()}})),
                    _ => None,
                })
                .collect();
            let mut v = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
            if !calls.is_empty() {
                v["tool_calls"] = json!(calls);
            }
            v
        }
        Message::ToolResult(t) => json!({"role": "tool", "tool_call_id": t.tool_call_id, "content": tool_result_text(&t.content)}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage, UserMessage};

    fn assistant(content: Vec<AssistantContent>, stop: StopReason) -> AgentMessage {
        AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content,
            api: "openai-completions".into(),
            provider: "soket".into(),
            model: "gpt".into(),
            response_model: None,
            response_id: None,
            usage: Usage { input: 100, output: 10, ..Usage::empty() },
            stop_reason: stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        }))
    }

    fn text(t: &str) -> AssistantContent {
        AssistantContent::Text(TextContent { text: t.into(), text_signature: None })
    }

    /// One user message -> tool call -> tool result -> final reply: two model requests, the
    /// second of which must carry the tool call and its result in its `messages`.
    #[test]
    fn rebuilds_each_model_request_with_tool_history() {
        let r = Recorder::default();
        let now = Utc::now();
        let prior = vec![AgentMessage::Llm(Message::User(UserMessage { content: UserMessageContent::Text("Hey".into()), timestamp: 0 }))];
        let user = AgentMessage::Llm(Message::User(UserMessage { content: UserMessageContent::Text("9737954125".into()), timestamp: 0 }));
        let call = assistant(
            vec![
                AssistantContent::Thinking(ThinkingContent { thinking: "store it".into(), thinking_signature: None, redacted: None }),
                AssistantContent::ToolCall(ToolCall { id: "c1".into(), name: "store_user_detail".into(), arguments: json!({"value": "9737954125"}), thought_signature: None }),
            ],
            StopReason::ToolUse,
        );
        let result = AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "store_user_detail".into(),
            content: vec![ToolResultContent::Text(TextContent { text: "{\"success\":true}".into(), text_signature: None })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
        }));
        let reply = assistant(vec![text("Please enter the OTP.")], StopReason::Stop);
        {
            let mut ev = r.events.lock();
            ev.push((now, Rec::TurnStart));
            ev.push((now, Rec::Msg(user)));
            ev.push((now, Rec::Msg(call)));
            ev.push((now, Rec::ToolStart { id: "c1".into(), name: "store_user_detail".into(), args: json!({"value": "9737954125"}) }));
            ev.push((now, Rec::ToolEnd { id: "c1".into(), result: json!({"success": true}), is_error: false }));
            ev.push((now, Rec::Msg(result)));
            ev.push((now, Rec::TurnStart));
            ev.push((now, Rec::Msg(reply)));
        }
        let (steps, runs) = r.finish("SYS", prior);
        assert_eq!(steps.len(), 2);
        let roles = |s: &Step| s.messages.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(roles(&steps[0]), ["system", "user", "user"]);
        assert_eq!(steps[0].output["tool_calls"][0]["function"]["name"], "store_user_detail");
        assert_eq!(steps[0].thinking, "store it");
        assert_eq!(steps[0].stop_reason, "ToolUse");
        assert_eq!(roles(&steps[1]), ["system", "user", "user", "assistant", "tool"]);
        assert_eq!(steps[1].messages[4]["tool_call_id"], "c1");
        assert_eq!(steps[1].output["content"], "Please enter the OTP.");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].name, "store_user_detail");
        assert_eq!(runs[0].result, json!({"success": true}));
    }
}
