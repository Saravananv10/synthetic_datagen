//! `loop-group-conversation-gen` — N-way group conversation synthetic dataset
//! generator. A separate binary from `loop-conversation-gen` (the 1-on-1
//! pipeline in `../main.rs`, untouched by this addition). Reuses the
//! stateless utility files (`model_util.rs`, `web_search.rs`) as source via
//! `#[path]` rather than a workspace lib — no shared crate, no risk to the
//! original binary's behavior. See
//! `/home/saravanan/.claude/plans/happy-launching-flame.md` for the design.

#[path = "../model_util.rs"]
mod model_util;
#[path = "../web_search.rs"]
mod web_search;
#[path = "../group_spec.rs"]
mod group_spec;
#[path = "../group_trace.rs"]
mod group_trace;
#[path = "../group_conversation.rs"]
mod group_conversation;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use loop_app_core::config::auth::FileCredentialStore;
use loop_app_core::config::paths::{auth_path, ensure_agent_dirs, get_agent_dir};
use loop_app_core::runtime::{build_models, ensure_provider_connected};
use loop_ai::providers::SOKET_PROVIDER_ID;
use loop_ai::ModelsRefreshOptions;

#[derive(Debug, Parser)]
#[command(
    name = "loop-group-conversation-gen",
    about = "N-way group conversation synthetic dataset generator"
)]
struct Cli {
    /// A single GroupConversationSpec JSON file, or a directory of them.
    #[arg(long)]
    spec: PathBuf,

    /// Directory to write per-conversation trace JSON files into.
    #[arg(long, default_value = "artifacts/group_conversation_dataset/traces")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

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

    let refresh = models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(true),
            force: true,
            provider_id: Some(SOKET_PROVIDER_ID.into()),
        })
        .await;
    for (pid, err) in &refresh.errors {
        tracing::warn!("model refresh {pid}: {err}");
    }

    let specs = group_spec::load_specs(&cli.spec)?;
    println!("Loaded {} group conversation spec(s)", specs.len());

    let state_dir = agent_dir.join("group-conversation-gen-state");
    std::fs::create_dir_all(&state_dir)?;

    let mut ok = 0usize;
    let mut failed = 0usize;
    for spec in &specs {
        println!(
            "\n=== {} — {} persona(s), {} round(s) each, order={:?} ===",
            spec.id,
            spec.personas.len(),
            spec.rounds_per_persona,
            spec.turn_order
        );
        let result =
            group_conversation::run_group_conversation(spec, Arc::clone(&models), &state_dir)
                .await;
        match result {
            Ok(trace) => {
                let path = trace.write_to(&cli.out)?;
                let turns = trace.transcript.len();
                println!(
                    "  -> {} ({} turns, status={})",
                    path.display(),
                    turns,
                    trace.status
                );
                if trace.status == "completed" {
                    ok += 1;
                } else {
                    failed += 1;
                }
            }
            Err(e) => {
                println!("  -> ERROR: {e}");
                failed += 1;
            }
        }
    }

    println!("\n=== {ok} completed, {failed} failed (of {}) ===", specs.len());
    Ok(())
}
