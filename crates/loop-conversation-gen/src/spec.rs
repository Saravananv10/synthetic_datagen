//! `ConversationSpec` — one file describes one conversation to generate: an
//! opening line plus 2+ personas that take turns round-robin.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationSpec {
    pub id: String,
    /// The opening line — e.g. the asker's actual question, not an abstract
    /// "topic label". Handed to the first persona as-is on round 1.
    pub topic: String,
    /// One round = every persona speaks once. "Longer conversations" per the
    /// brief — pick a generous default when authoring specs (8-12+).
    pub rounds_per_persona: u32,
    /// Speaking order = list order, round-robin. 2+ entries.
    pub personas: Vec<PersonaSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PersonaSpec {
    pub name: String,
    /// `provider/model_id`, e.g. "soket/gpt-oss-120b" — same convention as
    /// `resolve_model_spec` in loop-datagen-server's subagents.rs.
    pub model: String,
    pub system_prompt: String,
    /// Per-persona opt-in to the `web_search` tool.
    #[serde(default)]
    pub allow_web_search: bool,
}

/// Load one spec file, or every `*.json`/`*.jsonl` file in a directory
/// (non-recursive). `.jsonl` support is additive — one spec per line,
/// for the multilingual task-bank shards (millions of specs is not a
/// one-file-per-spec-friendly count; existing `.json` single-spec files
/// behave exactly as before).
pub fn load_specs(path: &Path) -> anyhow::Result<Vec<ConversationSpec>> {
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

fn load_spec_path(path: &Path) -> anyhow::Result<Vec<ConversationSpec>> {
    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        load_jsonl_file(path)
    } else {
        Ok(vec![load_spec_file(path)?])
    }
}

fn load_jsonl_file(path: &Path) -> anyhow::Result<Vec<ConversationSpec>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut specs = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let spec: ConversationSpec = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("parse {}:{}: {e}", path.display(), i + 1))?;
        if spec.personas.len() < 2 {
            anyhow::bail!("{}:{}: needs at least 2 personas", path.display(), i + 1);
        }
        specs.push(spec);
    }
    Ok(specs)
}

fn load_spec_file(path: &Path) -> anyhow::Result<ConversationSpec> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let spec: ConversationSpec = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    if spec.personas.len() < 2 {
        anyhow::bail!("{}: needs at least 2 personas", path.display());
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_jsonl_shard() {
        let dir = std::env::temp_dir().join(format!("loop-conv-spec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shard.jsonl");
        let line = |id: &str| {
            serde_json::json!({
                "id": id, "topic": "t", "rounds_per_persona": 2,
                "personas": [
                    {"name": "A", "model": "soket/gpt", "system_prompt": "p"},
                    {"name": "B", "model": "soket/gpt", "system_prompt": "p"}
                ]
            })
            .to_string()
        };
        std::fs::write(&path, format!("{}\n{}\n", line("a"), line("b"))).unwrap();

        let specs = load_specs(&path).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "a");
        assert_eq!(specs[1].id, "b");
        assert_eq!(specs[0].personas.len(), 2);

        let dir_specs = load_specs(&dir).unwrap();
        assert_eq!(dir_specs.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
