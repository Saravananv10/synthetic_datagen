//! Session registry: maps the opaque `session_id` string
//! (`ChatRequest.session_id`, minted client-side by `adapter/loop/session.py`
//! exactly like `adapter/chakra/session.py` does today) to a resident
//! [`Conversation`], with a small on-disk sidecar so a server restart can
//! still resume a session by id via `AgentHarness::switch_session`-style
//! rehydration (here: `SessionRepository::open` on the mapped internal id).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use loop_agent::harness::{
    AgentHarness, AgentHarnessOptions, AgentHarnessResources, ExecutionEnv, HostExecutionEnv,
    SandboxMode, SessionRepository,
};
use loop_ai::{Model, Models};
use parking_lot::Mutex as SyncMutex;

use crate::conversation::Conversation;
use crate::prompts;
use crate::subagents::{self, SubagentEnv, SubagentModelConfig};

pub struct ConversationRegistry {
    resident: DashMap<String, Arc<Conversation>>,
    id_map: SyncMutex<IdMap>,
    id_map_path: PathBuf,
    models: Arc<Models>,
    session_repo: Arc<SessionRepository>,
    default_model: Model,
    model_config: SubagentModelConfig,
}

type IdMap = HashMap<String, String>;

impl ConversationRegistry {
    pub fn new(
        models: Arc<Models>,
        session_repo: Arc<SessionRepository>,
        default_model: Model,
        model_config: SubagentModelConfig,
        state_dir: &Path,
    ) -> Self {
        let id_map_path = state_dir.join("session_id_map.json");
        let id_map = load_id_map(&id_map_path);
        Self {
            resident: DashMap::new(),
            id_map: SyncMutex::new(id_map),
            id_map_path,
            models,
            session_repo,
            default_model,
            model_config,
        }
    }

    /// Resolve a `ChatRequest.model` override (`provider/model_id`, or a
    /// bare id assumed to be on the `soket` provider).
    pub fn resolve_model(&self, spec: &str) -> Option<Model> {
        subagents::resolve_model_spec(&self.models, spec)
    }

    /// Get a resident conversation, or build one (rehydrating from the
    /// session store when the client's `session_id` maps to a
    /// previously-seen internal session, else creating a fresh session).
    pub async fn get_or_create(
        &self,
        client_session_id: &str,
        cwd: &str,
    ) -> anyhow::Result<Arc<Conversation>> {
        if let Some(existing) = self.resident.get(client_session_id) {
            return Ok(Arc::clone(&existing));
        }

        let internal_id = self.id_map.lock().get(client_session_id).cloned();
        let session = match internal_id {
            Some(id) => match self.session_repo.open(&id).await {
                Ok(s) => s,
                Err(_) => self.session_repo.create(Some(cwd.to_string()), None).await?,
            },
            None => self.session_repo.create(Some(cwd.to_string()), None).await?,
        };

        let internal_id = session.metadata().id.clone();
        {
            let mut map = self.id_map.lock();
            map.insert(client_session_id.to_string(), internal_id);
            save_id_map(&self.id_map_path, &map);
        }

        let host_env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(Path::new(cwd)));
        let subagent_env = SubagentEnv {
            models: Arc::clone(&self.models),
            host_env: Arc::clone(&host_env),
            cwd: cwd.to_string(),
            model_config: self.model_config.clone(),
            session_repo: Arc::clone(&self.session_repo),
        };

        let mut tools = subagents::coordinator_base_tools(Arc::clone(&host_env));
        tools.push(subagents::build_agent_tool(subagent_env));

        let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
            models: Arc::clone(&self.models),
            model: Some(self.default_model.clone()),
            session,
            host_env,
            tools,
            system_prompt: prompts::coordinator_system_prompt(),
            sandbox: SandboxMode::Disabled,
            resources: AgentHarnessResources {
                skills: Vec::new(),
                prompt_templates: Vec::new(),
            },
        }));

        let conversation = Conversation::new(client_session_id.to_string(), harness);
        self.resident
            .insert(client_session_id.to_string(), Arc::clone(&conversation));
        Ok(conversation)
    }
}

fn load_id_map(path: &Path) -> IdMap {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_id_map(path: &Path, map: &IdMap) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, raw);
    }
}
