//! gRPC agent server for the headless-harness datagen pipeline, backed by
//! `loop-agent`'s `AgentHarness` — a chakra.proto-compatible replacement for
//! the Chakra backend. See `/home/saravanan/.claude/plans/happy-launching-flame.md`
//! (or the repo's plan doc, if committed) for the full design.

mod approval;
pub(crate) mod conversation;
mod prompts;
mod registry;
mod service;
mod subagents;

pub mod pb {
    tonic::include_proto!("loopagent.v1");
}

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use loop_app_core::config::auth::FileCredentialStore;
use loop_app_core::config::paths::{auth_path, ensure_agent_dirs, get_agent_dir, sessions_db_path};
use loop_app_core::runtime::{build_models, ensure_provider_connected};
use loop_agent::harness::{create_session_repository, create_sqlite_session_store};
use loop_ai::providers::{SOKET_DEFAULT_MODEL_ID, SOKET_PROVIDER_ID};
use tonic::transport::Server;

use crate::registry::ConversationRegistry;
use crate::service::AgentServiceImpl;
use crate::subagents::SubagentModelConfig;

#[derive(Debug, Parser)]
#[command(name = "loop-datagen-server", about = "gRPC agent backend for headless-harness datagen")]
struct Cli {
    /// Host:port to listen on.
    #[arg(long, default_value = "0.0.0.0:50061")]
    listen: String,

    /// Require interactive approval (`ActionRequired`/`UserInput`) for
    /// `bash` calls instead of auto-approving them. Datagen runs should
    /// leave this off (the default): the subagent prompts are already
    /// written for zero human interaction. Useful for interactive/manual
    /// use of the server, or for testing the approval bridge itself.
    #[arg(long)]
    require_approval: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let agent_dir = get_agent_dir();
    ensure_agent_dirs(&agent_dir)?;

    let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
    ensure_provider_connected(&credentials, &[], false)?;

    let models = build_models(&agent_dir, Arc::clone(&credentials), &[])?;
    let default_model = models
        .get_model(SOKET_PROVIDER_ID, SOKET_DEFAULT_MODEL_ID)
        .or_else(|| models.get_models(None).into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("no models available"))?;

    let model_config = SubagentModelConfig::from_env(&models, &default_model);

    let sessions_db: PathBuf = sessions_db_path(&agent_dir);
    let store = create_sqlite_session_store(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let session_repo = Arc::new(create_session_repository(store, None));

    let registry = Arc::new(ConversationRegistry::new(
        models,
        session_repo,
        default_model,
        model_config,
        &agent_dir,
    ));

    let addr = cli.listen.parse()?;
    let auto_approve = !cli.require_approval;
    tracing::info!("loop-datagen-server listening on {addr} (auto_approve={auto_approve})");

    Server::builder()
        .add_service(pb::agent_service_server::AgentServiceServer::new(
            AgentServiceImpl {
                registry,
                auto_approve,
            },
        ))
        .serve(addr)
        .await?;

    Ok(())
}
