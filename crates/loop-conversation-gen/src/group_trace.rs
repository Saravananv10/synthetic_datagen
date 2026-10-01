//! Trace/output types for the group-conversation pipeline — a separate type
//! family from `trace.rs`'s (that file is untouched by this pipeline).

use std::collections::HashMap;

use serde::Serialize;

use crate::group_spec::GroupConversationSpec;

#[derive(Debug, Clone, Serialize)]
pub struct GroupToolCallRecord {
    pub tool: String,
    pub query: String,
    pub result_preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupTurnUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupTurnRecord {
    /// 1-based overall turn index (not "round" — with random turn order,
    /// and with N>2 personas, "round" doesn't map cleanly onto a single
    /// speaker index the way the 1-on-1 pipeline's did).
    pub turn: u32,
    pub persona: String,
    pub text: String,
    pub tool_calls: Vec<GroupToolCallRecord>,
    pub usage: GroupTurnUsage,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupPersonaSummary {
    pub name: String,
    pub model: String,
    pub allow_web_search: bool,
    pub team: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupConversationTrace {
    pub conversation_id: String,
    pub topic: String,
    pub turn_order: String,
    pub created_at: String,
    pub personas: Vec<GroupPersonaSummary>,
    pub transcript: Vec<GroupTurnRecord>,
    pub status: String,
    pub error: Option<String>,
    pub usage_totals: HashMap<String, GroupTurnUsage>,
}

impl GroupConversationTrace {
    pub fn new(spec: &GroupConversationSpec) -> Self {
        Self {
            conversation_id: spec.id.clone(),
            topic: spec.topic.clone(),
            turn_order: format!("{:?}", spec.turn_order),
            created_at: chrono::Utc::now().to_rfc3339(),
            personas: spec
                .personas
                .iter()
                .map(|p| GroupPersonaSummary {
                    name: p.name.clone(),
                    model: p.model.clone(),
                    allow_web_search: p.allow_web_search,
                    team: p.team.clone(),
                })
                .collect(),
            transcript: Vec::new(),
            status: "running".to_string(),
            error: None,
            usage_totals: HashMap::new(),
        }
    }

    pub fn record_turn(&mut self, turn: GroupTurnRecord) {
        let totals = self
            .usage_totals
            .entry(turn.persona.clone())
            .or_insert(GroupTurnUsage { input: 0, output: 0 });
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
