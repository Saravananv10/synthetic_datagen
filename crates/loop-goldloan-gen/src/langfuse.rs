//! Langfuse trace export (`--langfuse`), via the native ingestion API
//! (`POST {base_url}/api/public/ingestion`, HTTP Basic auth with
//! `public_key:secret_key`, batched `{type, timestamp, body}` events) rather
//! than a full OTEL SDK — simpler for a batch CLI, and this is Langfuse's own
//! wire format.
//!
//! Layout, matching what the Soket LiteLLM proxy's own logger writes to its
//! "LLM Logging" project, so the two read the same:
//! - separate **sessions** per scenario run so the sides never mingle:
//!   `<scenario>-<seed>-<run_id>-agent` (stage agents + vision extraction),
//!   `<scenario>-<seed>-<run_id>-human` (the simulated human) and
//!   `<scenario>-<seed>-<run_id>-conversation` (the run summary, both sides);
//! - one **trace per model call** (a stage agent's turn, the human-sim's turn, one
//!   document's vision extraction): input `{system, prompt}`, the parsed output,
//!   and inside it an `agent-response` generation (model, tokens, latency), an
//!   `agent-thinking` span when the model returned reasoning, one `tool:<name>`
//!   span per tool the turn ran, and a closing `agent-stopped` event;
//! - one `conversation` trace per run, sent when the run ends: the whole
//!   human/agent chat as input and the outcome as output.
//!
//! Each model call's events go in ONE ingestion batch, so a trace and its
//! observations land (or fail) together — no orphaned observations. All sends
//! are fire-and-forget (spawned, logged on failure, never propagated) so
//! Langfuse being slow or down never affects the simulator itself.

use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};

/// POST one ingestion batch, retrying up to `max_attempts` times with backoff. `Ok(())` only
/// when the batch-ingestion endpoint's response body itself confirms no error for any event -
/// its HTTP status alone is not trustworthy (see the comment below).
async fn send_with_retries(
    http: &reqwest::Client,
    url: &str,
    auth: &str,
    events: &[(&'static str, String)],
    max_attempts: u32,
) -> Result<(), String> {
    // Bodies arrive already serialized (see `Body`), so the batch is assembled as text: going
    // back through `Value` would re-sort every object's keys alphabetically.
    let now = Utc::now().to_rfc3339();
    let batch = format!(
        r#"{{"batch":[{}]}}"#,
        events
            .iter()
            .map(|(t, body)| format!(r#"{{"id":"{}","type":"{t}","timestamp":"{now}","body":{body}}}"#, uuid::Uuid::new_v4()))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut last = String::new();
    for attempt in 1..=max_attempts {
        match http.post(url).header("Authorization", auth).header("Content-Type", "application/json").body(batch.clone()).send().await {
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                // The batch-ingestion endpoint returns 207 (in reqwest's 2xx "success" range)
                // even when THIS event failed: {"successes":[...],"errors":[...]}. Found live:
                // an orphan generation-create (traceId whose trace-create had failed) got HTTP
                // 500 outright, but under real load some events instead came back 207 with a
                // per-event entry in "errors" that a bare status-code check silently treated as
                // success - the actual root cause of a batch that looked clean (no warnings
                // logged) but had zero events land. The body must always be parsed.
                let body_says_error = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("errors").and_then(|e| e.as_array().map(|a| !a.is_empty())))
                    .unwrap_or(false);
                if status.is_success() && !body_says_error {
                    return Ok(());
                }
                last = format!("HTTP {status}: {}", text.chars().take(300).collect::<String>());
            }
            Err(e) => last = e.to_string(),
        }
        if attempt < max_attempts {
            tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
        }
    }
    Err(last)
}

/// An event body as JSON text. `serde_json::Value` objects always serialize with their keys
/// sorted (no `preserve_order` feature in this workspace), which put `prompt` above `system` in
/// Langfuse's input view; bodies that need a fixed key order serialize a struct instead.
fn body(v: &impl serde::Serialize) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

/// A trace's input, in the order Langfuse should show it: system prompt, the prompt, then the
/// tool definitions the model was given (omitted when it had none).
#[derive(serde::Serialize)]
struct SystemThenPrompt<'a> {
    system: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "is_empty_array")]
    tools: &'a Value,
}

fn is_empty_array(v: &&Value) -> bool {
    v.as_array().map_or(true, |a| a.is_empty())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceBody<'a, I: serde::Serialize> {
    id: &'a str,
    name: &'a str,
    session_id: &'a str,
    timestamp: &'a str,
    input: I,
    output: &'a Value,
    metadata: &'a Value,
    tags: Vec<String>,
}

pub struct LangfuseClient {
    http: reqwest::Client,
    base_url: String,
    auth_header: String,
    run_id: String,
}

/// One model request inside a turn: exactly the `messages` it was sent and the assistant
/// message it returned (text and/or `tool_calls`).
pub struct Step {
    pub messages: Value,
    pub output: Value,
    pub thinking: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub start: chrono::DateTime<Utc>,
    pub end: chrono::DateTime<Utc>,
    pub stop_reason: String,
    pub error: Option<String>,
}

/// One tool execution the harness ran between model requests.
pub struct ToolRun {
    pub call_id: String,
    pub name: String,
    pub args: Value,
    pub result: Value,
    pub is_error: bool,
    pub start: chrono::DateTime<Utc>,
    pub end: chrono::DateTime<Utc>,
}

/// One agent/human/vision turn, exported as its own trace (see the module doc for the layout).
pub struct ModelCall<'a> {
    /// Session this call belongs to: `<scenario>-<seed>-<run_id>-agent` or `…-human`.
    pub session_id: &'a str,
    /// Trace name, e.g. `stage1-agent`, `human`, `vision-pan`.
    pub name: String,
    pub model: &'a str,
    pub system: &'a str,
    pub prompt: &'a str,
    /// The OpenAI-format tool definitions sent with every request of this turn (may be empty).
    pub tools: Value,
    /// The turn's final, parsed output.
    pub output: Value,
    pub steps: Vec<Step>,
    pub tool_runs: Vec<ToolRun>,
    pub start: chrono::DateTime<Utc>,
    pub end: chrono::DateTime<Utc>,
    pub metadata: Value,
}

impl LangfuseClient {
    /// `None` when no `--langfuse` flag was passed; construction fails loudly
    /// (the caller should bail) when the flag was passed but the env vars
    /// (`LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY` / `LANGFUSE_BASE_URL`,
    /// loaded from `goldloan-datagen/.env`) are missing.
    pub fn from_env(run_id: &str) -> anyhow::Result<Self> {
        let public = std::env::var("LANGFUSE_PUBLIC_KEY").map_err(|_| anyhow::anyhow!("LANGFUSE_PUBLIC_KEY not set"))?;
        let secret = std::env::var("LANGFUSE_SECRET_KEY").map_err(|_| anyhow::anyhow!("LANGFUSE_SECRET_KEY not set"))?;
        let base_url = std::env::var("LANGFUSE_BASE_URL").map_err(|_| anyhow::anyhow!("LANGFUSE_BASE_URL not set"))?;
        Ok(Self::new(public, secret, base_url, run_id.to_string()))
    }

    pub fn new(public_key: String, secret_key: String, base_url: String, run_id: String) -> Self {
        use base64::Engine;
        let auth_header = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{public_key}:{secret_key}")));
        // A bare `reqwest::Client::new()` sets NO request timeout: found live, at scale, sending
        // against a self-hosted Langfuse instance under real load - a slow/overloaded response
        // (matching the 504s this instance does return under load) hangs the request forever
        // instead of failing visibly, so it neither logs a warning nor ever lands. Silent, total
        // data loss with zero error signal. An explicit timeout, plus one short retry for exactly
        // the transient 5xx this instance showed (confirmed live: an identical payload that got a
        // 500 succeeded outright on immediate retry), fixes both the hang and the flakiness.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(8))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        LangfuseClient { http, base_url: base_url.trim_end_matches('/').to_string(), auth_header, run_id }
    }

    /// Fire-and-forget: spawn the retrying send, log a warning if all attempts fail, never
    /// propagate anything to the caller.
    fn send(&self, what: String, events: Vec<(&'static str, String)>) {
        let (http, url, auth) = (self.http.clone(), format!("{}/api/public/ingestion", self.base_url), self.auth_header.clone());
        tokio::spawn(async move {
            // Found live: several workers writing at the same instant get genuine 500s from this
            // instance's ingestion endpoint; a small random stagger spreads them out so most
            // never collide, rather than relying entirely on retrying after the fact.
            let jitter_ms = rand::Rng::gen_range(&mut rand::thread_rng(), 0..1500);
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
            if let Err(last) = send_with_retries(&http, &url, &auth, &events, 4).await {
                tracing::warn!("langfuse {what}: giving up after 4 attempts: {last}");
            }
        });
    }

    fn tags(&self, role: &str) -> Vec<String> {
        vec!["goldloan".into(), role.into(), format!("run:{}", self.run_id)]
    }

    /// Export one turn as its own trace: a generation per model request (full `messages` +
    /// `tools` in, assistant message out), its reasoning, each tool run, and a stop event.
    pub fn model_call(&self, c: ModelCall<'_>) {
        let trace_id = uuid::Uuid::new_v4().to_string();
        let role = if c.name.starts_with("stage") { "agent" } else if c.name.starts_with("vision") { "vision" } else { "human" };
        let mut metadata = c.metadata;
        if let Some(m) = metadata.as_object_mut() {
            m.insert("run_id".into(), json!(self.run_id));
            m.insert("model_requests".into(), json!(c.steps.len()));
            m.insert("tool_runs".into(), json!(c.tool_runs.len()));
        }
        let mut events: Vec<(&'static str, String)> = vec![(
            "trace-create",
            body(&TraceBody {
                id: &trace_id,
                name: &c.name,
                session_id: c.session_id,
                timestamp: &c.start.to_rfc3339(),
                input: SystemThenPrompt { system: c.system, prompt: c.prompt, tools: &c.tools },
                output: &c.output,
                metadata: &metadata,
                tags: self.tags(role),
            }),
        )];
        let n = c.steps.len();
        for (i, st) in c.steps.iter().enumerate() {
            let (start, end) = (st.start.to_rfc3339(), st.end.to_rfc3339());
            let mut input = json!({"messages": st.messages});
            if !is_empty_array(&&c.tools) {
                input["tools"] = c.tools.clone();
            }
            let mut g = json!({"id": uuid::Uuid::new_v4().to_string(), "traceId": trace_id,
                   "name": if n > 1 { format!("agent-response {}/{n}", i + 1) } else { "agent-response".into() },
                   "model": c.model, "startTime": start, "endTime": end, "completionStartTime": end,
                   "input": input, "output": st.output,
                   "metadata": {"stop_reason": st.stop_reason},
                   "usage": {"input": st.input_tokens, "output": st.output_tokens, "total": st.input_tokens + st.output_tokens, "unit": "TOKENS"}});
            if let Some(e) = &st.error {
                g["level"] = json!("ERROR");
                g["statusMessage"] = json!(e);
            }
            events.push(("generation-create", body(&g)));
            if !st.thinking.trim().is_empty() {
                events.push((
                    "span-create",
                    body(&json!({"id": uuid::Uuid::new_v4().to_string(), "traceId": trace_id,
                           "name": if n > 1 { format!("agent-thinking {}/{n}", i + 1) } else { "agent-thinking".into() },
                           "startTime": start, "endTime": end, "output": st.thinking})),
                ));
            }
        }
        for r in &c.tool_runs {
            let mut sp = json!({"id": uuid::Uuid::new_v4().to_string(), "traceId": trace_id, "name": format!("tool:{}", r.name),
                   "startTime": r.start.to_rfc3339(), "endTime": r.end.to_rfc3339(), "input": r.args, "output": r.result,
                   "metadata": {"tool_call_id": r.call_id}});
            if r.is_error {
                sp["level"] = json!("ERROR");
            }
            events.push(("span-create", body(&sp)));
        }
        let stop = c.steps.last().map(|s| s.stop_reason.clone()).unwrap_or_else(|| "unknown".into());
        events.push((
            "event-create",
            body(&json!({"id": uuid::Uuid::new_v4().to_string(), "traceId": trace_id, "name": "agent-stopped",
                   "startTime": c.end.to_rfc3339(), "metadata": {"stop_reason": stop}})),
        ));
        self.send(format!("{} ({})", c.name, c.session_id), events);
    }

    /// Close out a run with one `conversation` trace: the whole human/agent chat as
    /// `[{role, content}]` (rendered as a chat) and the run's outcome as output. Timestamped at
    /// the run's start so it sorts first in the session.
    pub fn conversation(&self, session_id: &str, start: chrono::DateTime<Utc>, chat: Value, output: Value, metadata: Value, mut tags: Vec<String>) {
        tags.extend(self.tags("conversation"));
        self.send(
            format!("conversation ({session_id})"),
            vec![(
                "trace-create",
                body(&TraceBody {
                    id: &format!("{session_id}-conversation"),
                    name: "conversation",
                    session_id,
                    timestamp: &start.to_rfc3339(),
                    input: chat,
                    output: &output,
                    metadata: &metadata,
                    tags,
                }),
            )],
        );
    }
}

pub type Langfuse = Option<Arc<LangfuseClient>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_input_keeps_system_before_prompt() {
        let (out, meta) = (json!("ok"), json!({}));
        let s = body(&TraceBody {
            id: "t",
            name: "human",
            session_id: "s",
            timestamp: "2026-09-30T00:00:00Z",
            input: SystemThenPrompt { system: "SYS", prompt: "USER", tools: &json!([{"type": "function"}]) },
            output: &out,
            metadata: &meta,
            tags: vec![],
        });
        assert!(s.find("\"system\"").unwrap() < s.find("\"prompt\"").unwrap(), "{s}");
        assert!(s.find("\"prompt\"").unwrap() < s.find("\"tools\"").unwrap(), "{s}");
        assert!(s.contains("\"sessionId\":\"s\""), "{s}");
        serde_json::from_str::<Value>(&s).unwrap();
    }
}
