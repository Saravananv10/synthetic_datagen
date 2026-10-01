//! LLM-backed stage agents: one `AgentHarness` per stage (fresh session, the
//! stage's system prompt, only that stage's tools), created on first entry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarness, AgentHarnessOptions, AgentHarnessResources,
    ExecutionEnv, HostExecutionEnv, SandboxMode, SessionRepository,
};
use loop_ai::{Model, Models};
use parking_lot::Mutex;

use serde_json::json;

use crate::backend::Backend;
use crate::langfuse::{Langfuse, ModelCall};
use crate::lf_capture::Recorder;
use crate::model_util::{assistant_text, provider_error, resolve_model_spec};
use crate::reply::{collapse_repeats, looks_garbled, parse_reply, strip_glued_tail, AgentOut};
use crate::{stages, tools};

/// The Soket gateway returns bursts of 502s under sustained multi-turn load; ~90s of backoff rides them out.
const MAX_ATTEMPTS: u32 = 12;
/// Extra asks allowed when a reply comes back garbled (tools already ran; only the wording is redone).
const MAX_REPAIRS: u32 = 3;
const REPAIR_PROMPT: &str = "(Your last reply to the user was garbled or incomplete. Reply again with ONLY the clean message the user should see, following the stage instructions. Do not call any tools.)";

pub struct LlmAgents {
    models: Arc<Models>,
    model: Model,
    model_spec: String,
    repo: SessionRepository,
    host_env: Arc<dyn ExecutionEnv>,
    backend: Arc<Mutex<Backend>>,
    /// Per stage: the harness, plus a Langfuse recorder on it when `--langfuse` is on.
    harnesses: HashMap<u8, (AgentHarness, Option<Recorder>)>,
    tag: String,
    lf: Langfuse,
    lf_session: String,
}

impl LlmAgents {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        models: Arc<Models>,
        model_spec: &str,
        backend: Arc<Mutex<Backend>>,
        state_dir: &std::path::Path,
        tag: &str,
        lf: Langfuse,
        lf_session: String,
    ) -> anyhow::Result<Self> {
        let model = resolve_model_spec(&models, model_spec).ok_or_else(|| anyhow::anyhow!("unknown agent model: {model_spec}"))?;
        let store = create_sqlite_session_store(&state_dir.join("goldloan-agents.db")).map_err(|e| anyhow::anyhow!("sqlite: {e}"))?;
        let repo = create_session_repository(store, None);
        let scratch = std::env::temp_dir().join(format!("goldloan-agents-{tag}"));
        std::fs::create_dir_all(&scratch)?;
        let host_env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&scratch));
        Ok(LlmAgents { models, model, model_spec: model_spec.to_string(), repo, host_env, backend, harnesses: HashMap::new(), tag: tag.to_string(), lf, lf_session })
    }

    async fn harness(&mut self, stage: u8) -> anyhow::Result<&(AgentHarness, Option<Recorder>)> {
        if !self.harnesses.contains_key(&stage) {
            let session = self
                .repo
                .create(None, Some(format!("{}:stage{stage}", self.tag)))
                .await
                .map_err(|e| anyhow::anyhow!("create session: {e}"))?;
            let h = AgentHarness::new(AgentHarnessOptions {
                models: Arc::clone(&self.models),
                model: Some(self.model.clone()),
                session,
                host_env: Arc::clone(&self.host_env),
                tools: tools::agent_tools(stage, Arc::clone(&self.backend)),
                system_prompt: stages::system_prompt(stage),
                sandbox: SandboxMode::Disabled,
                resources: AgentHarnessResources { skills: Vec::new(), prompt_templates: Vec::new() },
            });
            let rec = self.lf.as_ref().map(|_| Recorder::attach(&h));
            self.harnesses.insert(stage, (h, rec));
        }
        Ok(self.harnesses.get(&stage).unwrap())
    }

    pub async fn respond(&mut self, stage: u8, input: &str) -> anyhow::Result<AgentOut> {
        let started = Instant::now();
        let wall_start = chrono::Utc::now();
        let backend = Arc::clone(&self.backend);
        let (lf, lf_session, model_spec) = (self.lf.clone(), self.lf_session.clone(), self.model_spec.clone());
        let (h, rec) = self.harness(stage).await?;
        let history = match rec {
            Some(r) => r.begin(h).await,
            None => vec![],
        };
        let (mut tin, mut tout) = (0u64, 0u64);
        let mut last_err = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let this = if attempt == 1 { input.to_string() } else { "(The previous attempt hit a transient error — please respond to the conversation so far.)".to_string() };
            match h.prompt(this).await {
                Ok(msg) => {
                    if let Some(e) = provider_error(&msg) {
                        last_err = e;
                    } else {
                        if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = &msg {
                            tin += am.usage.input;
                            tout += am.usage.output;
                        }
                        let raw = assistant_text(&msg);
                        let (text, mut options) = parse_reply(&raw);
                        let mut fixed = collapse_repeats(&text);
                        if let Some(head) = strip_glued_tail(&fixed) {
                            fixed = head;
                        }
                        let mut raw_text = if fixed != text { Some(text.clone()) } else { None };
                        // Quality gate. (1) The reply announces a step whose tool never succeeded: ask
                        // again with tools allowed. (2) The reply is garbled: redo only the wording.
                        for _ in 0..MAX_REPAIRS {
                            let missing = backend.lock().unbacked_claims(&fixed);
                            let prompt = if !missing.is_empty() {
                                format!(
                                    "(Your reply says something that is only true once `{}` has succeeded, but it has not. If the stage instructions call for that step, call the tool now; then reply with the correct message for the user.)",
                                    missing.join("`, `")
                                )
                            } else if looks_garbled(&fixed) {
                                REPAIR_PROMPT.to_string()
                            } else {
                                break;
                            };
                            raw_text.get_or_insert_with(|| fixed.clone());
                            match h.prompt(prompt).await {
                                Ok(m2) if provider_error(&m2).is_none() => {
                                    if let loop_agent::AgentMessage::Llm(loop_agent::Message::Assistant(am)) = &m2 {
                                        tin += am.usage.input;
                                        tout += am.usage.output;
                                    }
                                    let (t2, o2) = parse_reply(&assistant_text(&m2));
                                    fixed = collapse_repeats(&t2);
                                    if let Some(head) = strip_glued_tail(&fixed) {
                                        fixed = head;
                                    }
                                    options = o2;
                                }
                                _ => break,
                            }
                        }
                        if let (Some(c), Some(r)) = (&lf, rec) {
                            let system = stages::system_prompt(stage);
                            let (steps, tool_runs) = r.finish(&system, history);
                            c.model_call(ModelCall {
                                session_id: &lf_session,
                                name: format!("stage{stage}-agent"),
                                model: &model_spec,
                                system: &system,
                                prompt: input,
                                tools: json!(tools::definitions_for_stage(stage)),
                                output: json!({"text": fixed, "options": options, "repaired": raw_text.is_some()}),
                                steps,
                                tool_runs,
                                start: wall_start,
                                end: chrono::Utc::now(),
                                metadata: json!({"stage": stage}),
                            });
                        }
                        return Ok(AgentOut { text: fixed, raw_text, options, input_tokens: tin, output_tokens: tout, duration_ms: started.elapsed().as_millis() as u64 });
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
            tracing::warn!("stage {stage} agent: attempt {attempt}/{MAX_ATTEMPTS} failed: {}", last_err.lines().next().unwrap_or(""));
            tokio::time::sleep(std::time::Duration::from_secs((1u64 << (attempt - 1)).min(45))).await;
        }
        anyhow::bail!("agent model failed after {MAX_ATTEMPTS} attempts: {last_err}")
    }
}
