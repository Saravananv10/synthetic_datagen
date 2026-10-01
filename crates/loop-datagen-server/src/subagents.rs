//! Subagent dispatch: the `Agent` tool the coordinator harness calls, and
//! per-subagent-type model routing.
//!
//! `loop-agent` has no built-in Task-tool / subagent primitive: each
//! `AgentHarness` is strictly single-model / single-system-prompt /
//! single-turn. Subagents are therefore built as a layer on top: dispatching
//! an `Agent` tool call constructs a fresh, short-lived `AgentHarness` (its
//! own child session, its own model, its own tool set) and runs it to
//! completion, returning its final assistant text as the tool result.
//! Subagents never get the `Agent` tool themselves (one level of nesting
//! only) and never go through tool-approval hooks (they run fully
//! autonomously, matching the datagen pipeline's autonomy requirement).

use std::pin::Pin;
use std::sync::Arc;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_write_tool, AgentHarness,
    AgentHarnessOptions, AgentHarnessResources, ExecutionEnv, SandboxMode,
};
use loop_agent::types::{AgentMessage, AgentTool, AgentToolResult};
use loop_ai::{AssistantContent, Message, Model, Models};
use serde_json::Value;

use crate::prompts;

/// Canonical subagent roles. Wire-level `subagent_type` strings are
/// normalized into these — keep the exact string set headless-harness's
/// `controller/workflow_common.py` (`ALLOWED_AGENT_SPAWN_TYPES`) and
/// `controller/lifecycle.py` already hardcode, so no Python controller code
/// needs to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRole {
    Plan,
    GeneralPurpose,
    Explore,
    Verification,
}

impl SubagentRole {
    /// Normalize a raw `subagent_type` argument into a role, accepting the
    /// same aliases Python does (`generalPurpose`, `verify`).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "Plan" => Some(Self::Plan),
            "general-purpose" | "generalPurpose" => Some(Self::GeneralPurpose),
            "Explore" => Some(Self::Explore),
            "verification" | "verify" => Some(Self::Verification),
            _ => None,
        }
    }

    /// Canonical wire string (matches the strings Python already expects in
    /// the `Agent` tool call's `arguments_json.subagent_type`).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::GeneralPurpose => "general-purpose",
            Self::Explore => "Explore",
            Self::Verification => "verification",
        }
    }

    fn system_prompt(self) -> String {
        match self {
            Self::Plan => prompts::plan_system_prompt(),
            Self::GeneralPurpose => prompts::general_purpose_system_prompt(),
            Self::Explore => prompts::explore_system_prompt(),
            Self::Verification => prompts::verification_system_prompt(),
        }
    }

    /// Tool set for this role. Plan/Explore/verification are read-only (no
    /// write/edit) — matching Chakra's own `disallowedTools` restriction on
    /// these three roles; `bash` stays available for read-only commands per
    /// the role's own prompt instructions.
    fn tools(self, env: Arc<dyn ExecutionEnv>) -> Vec<AgentTool> {
        match self {
            Self::GeneralPurpose => vec![
                create_read_tool(Arc::clone(&env)),
                create_write_tool(Arc::clone(&env)),
                create_edit_tool(Arc::clone(&env)),
                create_bash_tool(env),
            ],
            Self::Plan | Self::Explore | Self::Verification => {
                vec![create_read_tool(Arc::clone(&env)), create_bash_tool(env)]
            }
        }
    }
}

/// Two-model routing: Plan + verification (quality gates) on a strong
/// model, general-purpose + Explore (bulk volume) on a fast/cheap model —
/// a real, always-on version of the split Chakra only ever expressed
/// internally (its `Explore` agent hardcoded to a cheap model, everything
/// else `inherit`).
#[derive(Clone)]
pub struct SubagentModelConfig {
    strong: Model,
    fast: Model,
}

impl SubagentModelConfig {
    /// Resolve from `LOOP_DATAGEN_STRONG_MODEL` / `LOOP_DATAGEN_FAST_MODEL`
    /// (`provider/model_id`, defaulting to the `soket` provider when no
    /// `/` is present), falling back to `default_model` for either/both
    /// when unset.
    pub fn from_env(models: &Models, default_model: &Model) -> Self {
        let strong = std::env::var("LOOP_DATAGEN_STRONG_MODEL")
            .ok()
            .and_then(|spec| resolve_model_spec(models, &spec))
            .unwrap_or_else(|| default_model.clone());
        let fast = std::env::var("LOOP_DATAGEN_FAST_MODEL")
            .ok()
            .and_then(|spec| resolve_model_spec(models, &spec))
            .unwrap_or_else(|| default_model.clone());
        Self { strong, fast }
    }

    fn model_for(&self, role: SubagentRole) -> Model {
        match role {
            SubagentRole::Plan | SubagentRole::Verification => self.strong.clone(),
            SubagentRole::GeneralPurpose | SubagentRole::Explore => self.fast.clone(),
        }
    }
}

pub(crate) fn resolve_model_spec(models: &Models, spec: &str) -> Option<Model> {
    let (provider, id) = spec
        .split_once('/')
        .unwrap_or((loop_ai::providers::SOKET_PROVIDER_ID, spec));
    models.get_model(provider, id)
}

/// Everything the dispatcher needs to build a subagent harness, shared with
/// the coordinator harness.
#[derive(Clone)]
pub struct SubagentEnv {
    pub models: Arc<Models>,
    pub host_env: Arc<dyn ExecutionEnv>,
    pub cwd: String,
    pub model_config: SubagentModelConfig,
    pub session_repo: Arc<loop_agent::harness::SessionRepository>,
}

/// Run one subagent turn to completion and return its final assistant text.
/// Never returns an empty string on success — this is what eliminates the
/// class of "subagent returned no text to parent" workarounds the Python
/// controller carries for the Chakra backend.
pub async fn run_subagent(
    env: SubagentEnv,
    role: SubagentRole,
    prompt: String,
) -> Result<String, String> {
    let session = env
        .session_repo
        .create(Some(env.cwd.clone()), Some(format!("subagent:{}", role.wire_name())))
        .await
        .map_err(|e| format!("create subagent session: {e}"))?;

    let harness = AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&env.models),
        model: Some(env.model_config.model_for(role)),
        session,
        host_env: Arc::clone(&env.host_env),
        tools: role.tools(Arc::clone(&env.host_env)),
        system_prompt: role.system_prompt(),
        sandbox: SandboxMode::Disabled,
        resources: AgentHarnessResources {
            skills: Vec::new(),
            prompt_templates: Vec::new(),
        },
    });

    let message = harness
        .prompt(prompt)
        .await
        .map_err(|e| format!("subagent turn failed: {e}"))?;

    if let Some(err) = crate::conversation::provider_error(&message) {
        return Err(format!("{} subagent: provider error: {err}", role.wire_name()));
    }

    let text = assistant_text(&message);
    if !text.trim().is_empty() {
        return Ok(text);
    }

    // The model finished its work via tool calls alone and closed with an
    // empty final message — a real, observed failure mode (seen with
    // qwen3-8-27b on general-purpose): the required status markers
    // (ENV_STATUS/IMPLEMENTATION_STATUS/RUNTIME_CHECK/VERDICT) never reach
    // Python's lifecycle observer, which then loops forever nudging a task
    // that actually finished. Force one explicit summary turn instead of
    // silently returning a placeholder that can never satisfy the marker
    // regexes the caller is waiting on.
    let followup = harness
        .prompt(
            "You finished without a closing summary. In plain text (no tool calls), \
             summarize what you did and end with the exact status line this role \
             requires (ENV_STATUS: READY / IMPLEMENTATION_STATUS: COMPLETE / \
             RUNTIME_CHECK: PASS / VERDICT: PASS|FAIL|PARTIAL / REPAIR_STATUS: COMPLETE \
             — whichever apply to the work you just did).",
        )
        .await
        .map_err(|e| format!("subagent summary follow-up failed: {e}"))?;
    if let Some(err) = crate::conversation::provider_error(&followup) {
        return Err(format!(
            "{} subagent: provider error on follow-up: {err}",
            role.wire_name()
        ));
    }
    let followup_text = assistant_text(&followup);
    if !followup_text.trim().is_empty() {
        return Ok(followup_text);
    }

    Ok(format!(
        "({} subagent completed its tool calls but produced no closing summary, \
         even after a follow-up request.)",
        role.wire_name()
    ))
}

fn assistant_text(message: &AgentMessage) -> String {
    if let AgentMessage::Llm(Message::Assistant(am)) = message {
        am.content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    } else {
        String::new()
    }
}

/// Base tool set for the coordinator harness (everything except `Agent`
/// itself, which the registry appends once it has a [`SubagentEnv`] to
/// close over).
///
/// Deliberately `read`-only, no `write`/`edit`/`bash` — the coordinator's
/// system prompt says it delegates everything, but a real run showed the
/// model will happily use a write tool directly (e.g. writing plan.md
/// itself right after Plan produced it) when one is available, which
/// silently bypasses the whole subagent-attribution/lifecycle-marker
/// mechanism Python's `controller/lifecycle.py` depends on. Removing the
/// tools makes delegation structural instead of a prompt suggestion.
pub fn coordinator_base_tools(env: Arc<dyn ExecutionEnv>) -> Vec<AgentTool> {
    vec![create_read_tool(env)]
}

/// Build the `Agent` tool registered on the coordinator harness only.
pub fn build_agent_tool(env: SubagentEnv) -> AgentTool {
    let parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "subagent_type": {
                "type": "string",
                "enum": ["Plan", "general-purpose", "Explore", "verification"],
                "description": "The type of specialized agent to use for this task"
            },
            "description": {
                "type": "string",
                "description": "A short (3-5 word) description of the task"
            },
            "prompt": {
                "type": "string",
                "description": "The task for the agent to perform"
            }
        },
        "required": ["subagent_type", "prompt"]
    });

    AgentTool {
        name: "Agent".to_string(),
        label: "Agent".to_string(),
        description:
            "Delegate work to a specialized subagent (Plan, general-purpose, Explore, verification)."
                .to_string(),
        parameters,
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_tool_call_id, args: Value, _cancel, _update_cb| {
            let env = env.clone();
            Box::pin(async move { execute_agent_tool(env, args).await })
                as Pin<Box<dyn std::future::Future<Output = Result<AgentToolResult, String>> + Send>>
        }),
    }
}

async fn execute_agent_tool(env: SubagentEnv, args: Value) -> Result<AgentToolResult, String> {
    let subagent_type = args
        .get("subagent_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing subagent_type".to_string())?;
    let role = SubagentRole::parse(subagent_type)
        .ok_or_else(|| format!("unknown subagent_type: {subagent_type}"))?;
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing prompt".to_string())?
        .to_string();

    let text = run_subagent(env, role, prompt).await?;

    Ok(AgentToolResult {
        content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent {
            text,
            text_signature: None,
        })],
        details: serde_json::json!({ "subagent_type": role.wire_name() }),
        usage: None,
        added_tool_names: None,
        terminate: None,
    })
}
