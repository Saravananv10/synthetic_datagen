//! Trace format. Mirrors the demo's snapshot layout (conversation -> turns ->
//! agents -> tool_calls with input/output, tokens, duration) and adds the
//! metadata needed for verification and dataset building.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::backend::{CallRecord, Session};

#[derive(Debug, Clone, Serialize)]
pub struct UserMsg {
    /// "operator" | "customer"
    pub role: String,
    /// What the UI shows (uploads render as "No input message provided." + attachments).
    pub text: String,
    pub attachments: Vec<String>,
    /// OTP SMS/email notifications the human saw before typing this message.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub device: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Tokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRec {
    pub name: String,
    pub arguments: Value,
    pub output: Value,
    pub clock_s: u64,
}

impl From<CallRecord> for ToolCallRec {
    fn from(c: CallRecord) -> Self {
        ToolCallRec { name: c.name, arguments: c.args, output: c.output, clock_s: c.clock_s }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRec {
    pub agent_name: String,
    /// What the agent actually received (for upload turns: the ingest summary text).
    pub input: String,
    pub tool_calls: Vec<ToolCallRec>,
    pub final_output: FinalOutput,
    pub tokens: Tokens,
    pub recorded_duration_ms: u64,
    /// Present only when the reply was mechanically repaired (repeated tail collapsed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinalOutput {
    pub text: String,
    pub structured_response: Value,
    pub process_completed: bool,
}

/// One uploaded document: what was really printed (`truth`), the raw text layer the
/// OCR/vision model saw, and what it extracted.
#[derive(Debug, Clone, Serialize)]
pub struct DocRec {
    pub doc: String,
    pub fixture: String,
    pub scan_text: String,
    pub truth: Value,
    pub extracted: Value,
}

/// The upload-ingest step that produced the agent's input for this turn.
#[derive(Debug, Clone, Serialize)]
pub struct IngestRec {
    pub kind: String,
    pub vision_model: String,
    pub docs: Vec<DocRec>,
    pub summary: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    pub turn: u32,
    pub stage: u8,
    pub clock_s: u64,
    pub user: UserMsg,
    pub agents: Vec<AgentRec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestRec>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub schema: &'static str,
    pub session_id: String,
    pub scenario_id: String,
    pub seed: u64,
    /// "scripted" (deterministic oracle agent + scripted human) or "llm".
    pub mode: String,
    pub models: Value,
    pub timing_is_simulated: bool,
    pub tool_definitions: Value,
    pub stage_prompts: Value,
    pub conversation: Vec<Turn>,
    pub status: String,
    pub outcome: String,
    pub final_state: Option<Session>,
    pub scenario: Value,
}

impl Trace {
    pub fn write_to(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", self.scenario_id));
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(path)
    }
}
