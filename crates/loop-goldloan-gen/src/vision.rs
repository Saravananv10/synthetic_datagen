//! The OCR / vision step. In `llm` mode GPT plays the OCR + document-understanding
//! model: it receives a document's raw text layer and returns the fields as JSON.
//! In `deterministic` mode the true fields are returned unchanged. A separate
//! middleware (`Backend::ingest_*_from`) validates the JSON and writes the summary.

use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarness, AgentHarnessOptions, AgentHarnessResources,
    ExecutionEnv, HostExecutionEnv, SandboxMode, SessionRepository,
};
use loop_ai::{Model, Models};
use serde_json::Value;

use crate::docs::{schema_for, Doc};
use crate::langfuse::{Langfuse, ModelCall, Step};
use crate::model_util::{assistant_text, assistant_thinking, provider_error, resolve_model_spec, stop_reason};

const VISION_SYSTEM: &str = "You are the OCR and document-understanding component of a loan-origination pipeline for Indian KYC and \
gold-loan documents. You receive the raw text layer of ONE scanned document (it may contain layout noise, Devanagari boilerplate or OCR \
errors). Extract the requested fields and reply with ONE JSON object and nothing else - no prose, no markdown fences.\n\
Rules: transcribe exactly what is printed; never correct, guess, normalise or invent values; use null for a field that is absent or \
unreadable; numbers are JSON numbers without units; keep identifiers (PAN, Aadhaar, certificate numbers) exactly as printed.";

pub struct VisionOut {
    pub fields: Value,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub enum Vision {
    Deterministic,
    Llm(LlmVision),
}

impl Vision {
    pub fn label(&self) -> String {
        match self {
            Vision::Deterministic => "deterministic".into(),
            Vision::Llm(v) => v.model_spec.clone(),
        }
    }

    pub async fn extract(&mut self, doc: &Doc, scan_text: &str) -> anyhow::Result<VisionOut> {
        match self {
            Vision::Deterministic => Ok(VisionOut { fields: doc.fields.clone(), input_tokens: 0, output_tokens: 0 }),
            Vision::Llm(v) => v.extract(doc, scan_text).await,
        }
    }
}

pub struct LlmVision {
    models: Arc<Models>,
    model: Model,
    model_spec: String,
    repo: SessionRepository,
    host_env: Arc<dyn ExecutionEnv>,
    lf: Langfuse,
    lf_session: String,
}

impl LlmVision {
    pub fn new(models: Arc<Models>, model_spec: &str, state_dir: &std::path::Path, tag: &str, lf: Langfuse, lf_session: String) -> anyhow::Result<Self> {
        let model = resolve_model_spec(&models, model_spec).ok_or_else(|| anyhow::anyhow!("unknown vision model: {model_spec}"))?;
        let store = create_sqlite_session_store(&state_dir.join("goldloan-vision.db")).map_err(|e| anyhow::anyhow!("sqlite: {e}"))?;
        let repo = create_session_repository(store, None);
        let scratch = std::env::temp_dir().join(format!("goldloan-vision-{tag}"));
        std::fs::create_dir_all(&scratch)?;
        let host_env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&scratch));
        Ok(LlmVision { models, model, model_spec: model_spec.to_string(), repo, host_env, lf, lf_session })
    }

    async fn extract(&mut self, doc: &Doc, scan_text: &str) -> anyhow::Result<VisionOut> {
        let wall_start = chrono::Utc::now();
        let prompt = format!(
            "DOCUMENT TYPE: {kind}\nReturn a JSON object with exactly these keys: {schema}\n\n--- SCAN TEXT ---\n{scan_text}\n--- END ---",
            kind = doc.kind,
            schema = schema_for(doc.kind)
        );
        let (mut tin, mut tout) = (0u64, 0u64);
        let mut last = String::new();
        for attempt in 1..=10u32 {
            // A fresh session per attempt: extractions are independent and must not see each other.
            let session = self.repo.create(None, Some(format!("vision:{}", doc.kind))).await.map_err(|e| anyhow::anyhow!("session: {e}"))?;
            let h = AgentHarness::new(AgentHarnessOptions {
                models: Arc::clone(&self.models),
                model: Some(self.model.clone()),
                session,
                host_env: Arc::clone(&self.host_env),
                tools: vec![],
                system_prompt: VISION_SYSTEM.to_string(),
                sandbox: SandboxMode::Disabled,
                resources: AgentHarnessResources { skills: Vec::new(), prompt_templates: Vec::new() },
            });
            match h.prompt(prompt.clone()).await {
                Ok(m) => {
                    if let Some(e) = provider_error(&m) {
                        last = e;
                    } else {
                        if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = &m {
                            tin += am.usage.input;
                            tout += am.usage.output;
                        }
                        let raw = assistant_text(&m);
                        match parse_json_object(&raw) {
                            Some(v) => {
                                if let Some(c) = &self.lf {
                                    // One request per extraction, on a fresh session: its messages are exactly these two.
                                    let messages = serde_json::json!([
                                        {"role": "system", "content": VISION_SYSTEM},
                                        {"role": "user", "content": &prompt},
                                    ]);
                                    let end = chrono::Utc::now();
                                    c.model_call(ModelCall {
                                        session_id: &self.lf_session,
                                        name: format!("vision-{}", doc.kind),
                                        model: &self.model_spec,
                                        system: VISION_SYSTEM,
                                        prompt: &prompt,
                                        tools: serde_json::json!([]),
                                        output: v.clone(),
                                        steps: vec![Step {
                                            messages,
                                            output: serde_json::json!({"role": "assistant", "content": raw}),
                                            thinking: assistant_thinking(&m),
                                            input_tokens: tin,
                                            output_tokens: tout,
                                            start: wall_start,
                                            end,
                                            stop_reason: stop_reason(&m),
                                            error: None,
                                        }],
                                        tool_runs: vec![],
                                        start: wall_start,
                                        end,
                                        metadata: serde_json::json!({"doc": doc.kind, "fixture": doc.fixture}),
                                    });
                                }
                                return Ok(VisionOut { fields: v, input_tokens: tin, output_tokens: tout });
                            }
                            None => last = format!("not a JSON object: {}", raw.chars().take(120).collect::<String>()),
                        }
                    }
                }
                Err(e) => last = e.to_string(),
            }
            tracing::warn!("vision {} attempt {attempt}/10: {}", doc.kind, last.lines().next().unwrap_or(""));
            tokio::time::sleep(std::time::Duration::from_secs((1u64 << (attempt - 1)).min(30))).await;
        }
        anyhow::bail!("vision model failed for {}: {last}", doc.kind)
    }
}

/// First `{ ... }` span of the text that parses as a JSON object.
pub fn parse_json_object(raw: &str) -> Option<Value> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&raw[start..=end]).ok().filter(|v| v.is_object())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_fenced_or_chatty_replies() {
        let v = parse_json_object("Sure!\n```json\n{\"a\": 1, \"b\": null}\n```").unwrap();
        assert_eq!(v["a"], 1);
        assert!(parse_json_object("no json here").is_none());
        assert!(parse_json_object("[1,2]").is_none());
    }
}
