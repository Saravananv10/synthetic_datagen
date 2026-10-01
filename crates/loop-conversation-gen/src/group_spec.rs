//! `GroupConversationSpec` — a NEW, separate spec type from `spec.rs`'s
//! `ConversationSpec` (that file is untouched by this pipeline). Describes an
//! N-persona group conversation. The grouping itself (2v2, 3v1, everyone on
//! the same side, ...) is declared per-persona via the free-form `team`
//! field — the engine (`group_conversation.rs`) never reads it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrder {
    #[default]
    RoundRobin,
    Random,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupConversationSpec {
    pub id: String,
    /// The opening line, handed to the first speaker as-is before anyone
    /// else has said anything.
    pub topic: String,
    /// How many turns each persona gets on average. Exact for
    /// `RoundRobin` (`total_turns = rounds_per_persona * personas.len()`,
    /// every persona speaks exactly this many times); expected-but-not-
    /// guaranteed for `Random` — a real property of random turn-taking,
    /// not a bug.
    pub rounds_per_persona: u32,
    #[serde(default)]
    pub turn_order: TurnOrder,
    /// 2+; the actual group size and composition (2v2, 3v1, all-together,
    /// anything else) live entirely in this list, not in any code.
    pub personas: Vec<GroupPersonaSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroupPersonaSpec {
    pub name: String,
    /// `provider/model_id`, same convention as the 1-on-1 pipeline's
    /// `PersonaSpec.model` (see `model_util::resolve_model_spec`).
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub allow_web_search: bool,
    /// Free-form label, entirely up to whoever authors the spec: "For"/
    /// "Against" for 2v2, "Majority"/"Contrarian" for 3v1, `null` on
    /// everyone for "just discussing together." Purely descriptive —
    /// carried into the trace as dataset metadata, never read by the turn
    /// loop.
    #[serde(default)]
    pub team: Option<String>,
}

/// Load one spec file, or every `*.json` file in a directory (non-recursive).
/// Same loading pattern as `spec::load_specs`, kept as a separate function
/// over the separate `GroupConversationSpec` type rather than generifying
/// `spec.rs`, per the isolation goal for this pipeline.
pub fn load_specs(path: &Path) -> anyhow::Result<Vec<GroupConversationSpec>> {
    if path.is_dir() {
        let mut specs = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("json") | Some("jsonl")
                )
            })
            .collect();
        entries.sort();
        for entry in entries {
            specs.extend(load_spec_path(&entry)?);
        }
        Ok(specs)
    } else {
        load_spec_path(path)
    }
}

fn load_spec_path(path: &Path) -> anyhow::Result<Vec<GroupConversationSpec>> {
    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        load_jsonl_file(path)
    } else {
        Ok(vec![load_spec_file(path)?])
    }
}

fn load_spec_file(path: &Path) -> anyhow::Result<GroupConversationSpec> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let spec: GroupConversationSpec = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    if spec.personas.len() < 2 {
        anyhow::bail!("{}: needs at least 2 personas", path.display());
    }
    Ok(spec)
}

/// Additive `.jsonl` support (one spec per line) — see `spec.rs::load_jsonl_file`
/// for the identical rationale (millions of specs at full multilingual-bank
/// scale is not a one-file-per-spec-friendly count).
fn load_jsonl_file(path: &Path) -> anyhow::Result<Vec<GroupConversationSpec>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut specs = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let spec: GroupConversationSpec = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("parse {}:{}: {e}", path.display(), i + 1))?;
        if spec.personas.len() < 2 {
            anyhow::bail!("{}:{}: needs at least 2 personas", path.display(), i + 1);
        }
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_jsonl_shard() {
        let dir = std::env::temp_dir().join(format!("loop-group-spec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shard.jsonl");
        let line = |id: &str| {
            serde_json::json!({
                "id": id, "topic": "t", "rounds_per_persona": 2, "turn_order": "round_robin",
                "personas": [
                    {"name": "A", "model": "soket/gpt", "system_prompt": "p", "team": null},
                    {"name": "B", "model": "soket/gpt", "system_prompt": "p", "team": null},
                    {"name": "C", "model": "soket/gpt", "system_prompt": "p", "team": null},
                    {"name": "D", "model": "soket/gpt", "system_prompt": "p", "team": null}
                ]
            })
            .to_string()
        };
        std::fs::write(&path, format!("{}\n{}\n", line("a"), line("b"))).unwrap();

        let specs = load_specs(&path).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "a");
        assert_eq!(specs[0].personas.len(), 4);
        assert_eq!(specs[0].turn_order, TurnOrder::RoundRobin);

        std::fs::remove_dir_all(&dir).ok();
    }
}
