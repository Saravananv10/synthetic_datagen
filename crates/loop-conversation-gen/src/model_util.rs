//! Small helpers duplicated from `loop-datagen-server`'s `subagents.rs` /
//! `conversation.rs` (both bin-only crates with nothing to depend on) —
//! model-spec resolution and the provider-error-surfacing check this
//! session's debugging found necessary twice against the Soket API.

use loop_ai::{Model, Models};

/// Resolve a `provider/model_id` spec string (bare id assumed to be on the
/// `soket` provider), same convention as loop-datagen-server's
/// `resolve_model_spec`.
pub fn resolve_model_spec(models: &Models, spec: &str) -> Option<Model> {
    let (provider, id) = spec
        .split_once('/')
        .unwrap_or((loop_ai::providers::SOKET_PROVIDER_ID, spec));
    models.get_model(provider, id)
}

/// A model call can return a nominally-`Ok` `AgentMessage` with empty
/// content and `stop_reason: Error` when the underlying provider call
/// actually failed (loop-ai reports provider errors this way rather than as
/// a Rust `Err`) — this session hit real 502/504s from the Soket API
/// surfaced exactly this way. Check this before trusting empty text.
pub fn provider_error(message: &loop_agent::AgentMessage) -> Option<String> {
    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = message {
        if matches!(
            am.stop_reason,
            loop_ai::StopReason::Error | loop_ai::StopReason::Aborted
        ) {
            return Some(am.error_message.clone().unwrap_or_else(|| {
                format!("model call ended with stop_reason {:?}", am.stop_reason)
            }));
        }
    }
    None
}

/// Extract the final assistant text.
pub fn assistant_text(message: &loop_agent::AgentMessage) -> String {
    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = message {
        let raw = am
            .content
            .iter()
            .filter_map(|c| match c {
                loop_ai::AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        sanitize_text(&raw)
    } else {
        String::new()
    }
}

/// Some models (observed live: `soket/gpt` writing non-Latin scripts, e.g.
/// Tamil) emit invisible non-standard space characters in place of most
/// regular spaces instead of plain ASCII space — one real conversation had
/// 8,107 U+202F (narrow no-break space) against only 3,480 real spaces from
/// that model, while a second model in the same conversation used none.
/// Normalize the space-like ones to a plain space and drop the zero-width
/// one outright so trace text is well-formed regardless of which model
/// produced it.
fn sanitize_text(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\u{202f}' | '\u{00a0}' | '\u{2009}' | '\u{2060}' => Some(' '),
            '\u{200b}' => None,
            other => Some(other),
        })
        .collect()
}
