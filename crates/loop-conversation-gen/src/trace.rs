//! Trace/output types — one JSON file per conversation.

use std::collections::HashMap;

use serde::Serialize;

use crate::spec::ConversationSpec;

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub query: String,
    pub result_preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnRecord {
    pub round: u32,
    pub persona: String,
    pub text: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub usage: TurnUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersonaSummary {
    pub name: String,
    pub model: String,
    pub allow_web_search: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationTrace {
    pub conversation_id: String,
    pub topic: String,
    pub created_at: String,
    pub personas: Vec<PersonaSummary>,
    pub transcript: Vec<TurnRecord>,
    pub status: String,
    pub error: Option<String>,
    pub usage_totals: HashMap<String, TurnUsage>,
}

impl ConversationTrace {
    pub fn new(spec: &ConversationSpec) -> Self {
        Self {
            conversation_id: spec.id.clone(),
            topic: spec.topic.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            personas: spec
                .personas
                .iter()
                .map(|p| PersonaSummary {
                    name: p.name.clone(),
                    model: p.model.clone(),
                    allow_web_search: p.allow_web_search,
                })
                .collect(),
            transcript: Vec::new(),
            status: "running".to_string(),
            error: None,
            usage_totals: HashMap::new(),
        }
    }

    pub fn record_turn(&mut self, turn: TurnRecord) {
        let totals = self
            .usage_totals
            .entry(turn.persona.clone())
            .or_insert(TurnUsage { input: 0, output: 0 });
        totals.input += turn.usage.input;
        totals.output += turn.usage.output;
        self.transcript.push(turn);
    }

    pub fn finish_ok(&mut self) {
        self.status = "completed".to_string();
    }

    pub fn finish_error(&mut self, message: String) {
        self.status = "error".to_string();
        self.error = Some(message);
    }

    pub fn write_to(&self, dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", self.conversation_id));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(path)
    }
}
