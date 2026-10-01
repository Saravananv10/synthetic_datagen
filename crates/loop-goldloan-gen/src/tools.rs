//! Tool schemas (`goldloan-datagen/spec/tools.json`) and their binding to the mock backend.

use std::sync::Arc;

use loop_agent::types::{AgentTool, AgentToolResult, ToolExecutionMode};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;

use crate::backend::Backend;

const TOOLS_JSON: &str = include_str!("../../../goldloan-datagen/spec/tools.json");

#[derive(Debug, Clone, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub stage: u8,
    pub provenance: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Deserialize)]
struct ToolsFile {
    tools: Vec<ToolSpec>,
}

pub fn specs() -> Vec<ToolSpec> {
    serde_json::from_str::<ToolsFile>(TOOLS_JSON).expect("tools.json").tools
}

pub fn specs_for_stage(stage: u8) -> Vec<ToolSpec> {
    specs().into_iter().filter(|t| t.stage == stage).collect()
}

/// OpenAI-style `tool_definitions` block, as it appears in the demo's trace file.
pub fn definitions_for_stage(stage: u8) -> Vec<Value> {
    specs_for_stage(stage)
        .into_iter()
        .map(|t| serde_json::json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
        .collect()
}

pub fn agent_tools(stage: u8, backend: Arc<Mutex<Backend>>) -> Vec<AgentTool> {
    specs_for_stage(stage)
        .into_iter()
        .map(|t| {
            let name = t.name.clone();
            let be = Arc::clone(&backend);
            let mut tool = AgentTool::simple(t.name.clone(), t.name.clone(), t.description.clone(), t.parameters.clone(), move |_id, args, _cancel, _upd| {
                let be = Arc::clone(&be);
                let name = name.clone();
                async move {
                    let out = be.lock().call(&name, &args);
                    Ok(AgentToolResult {
                        content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent { text: out.to_string(), text_signature: None })],
                        details: out,
                        usage: None,
                        added_tool_names: None,
                        terminate: None,
                    })
                }
            });
            // Steps depend on each other; never run a model's parallel tool calls concurrently.
            tool.execution_mode = Some(ToolExecutionMode::Sequential);
            tool
        })
        .collect()
}
