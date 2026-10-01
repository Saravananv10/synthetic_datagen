//! `loop-conversation-gen` — multi-persona long-conversation synthetic dataset
//! generator. Run one spec file, or every `*.json` in a directory, and write
//! a trace JSON per conversation. See
//! `/home/saravanan/.claude/plans/happy-launching-flame.md` for the design.

mod conversation;
mod model_util;
mod spec;
mod trace;
mod web_search;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use loop_app_core::config::auth::FileCredentialStore;
use loop_app_core::config::paths::{ensure_agent_dirs, get_agent_dir, auth_path};
use loop_app_core::runtime::{build_models, ensure_provider_connected};
use loop_ai::providers::SOKET_PROVIDER_ID;
use loop_ai::ModelsRefreshOptions;

#[derive(Debug, Parser)]
#[command(
    name = "loop-conversation-gen",
    about = "Multi-persona long-conversation synthetic dataset generator"
)]
struct Cli {
    /// A single ConversationSpec JSON file, or a directory of them.
    #[arg(long)]
    spec: PathBuf,

    /// Directory to write per-conversation trace JSON files into.
    #[arg(long, default_value = "artifacts/conversation_dataset/traces")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Loads TAVILY_API_KEY (and anything else) from a `.env` file in the repo
    // root if present. Env vars exported by hand in a shell don't survive
    // across sessions/new shells — a `.env` file does, and web_search.rs reads
    // TAVILY_API_KEY from the process environment at call time either way.
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
    // Seed catalog only has the default model — personas can reference any
    // Soket model, so refresh the full catalog synchronously before resolving.
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

    let specs = spec::load_specs(&cli.spec)?;
    println!("Loaded {} conversation spec(s)", specs.len());

    let state_dir = agent_dir.join("conversation-gen-state");
    std::fs::create_dir_all(&state_dir)?;

    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for (i, spec) in specs.iter().enumerate() {
        let trace_path = cli.out.join(format!("{}.json", spec.id));
        if already_completed(&trace_path) {
            skipped += 1;
            continue;
        }
        println!(
            "\n=== {} — {} persona(s), {} round(s) each ===",
            spec.id,
            spec.personas.len(),
            spec.rounds_per_persona
        );
        // Small gap between conversations. A prior 18h+ unattended run against
        // 420k specs with zero delay between conversations hit sustained
        // client-side transport errors; this isn't a full rate limiter, just
        // enough breathing room to avoid hammering the API back-to-back for
        // hours at a stretch.
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let result = conversation::run_conversation(spec, Arc::clone(&models), &state_dir).await;
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

    println!(
        "\n=== {ok} completed, {failed} failed, {skipped} already-completed skipped (of {}) ===",
        specs.len()
    );
    Ok(())
}

/// A prior run's trace already exists and completed successfully — skip
/// re-running it. Missing files and non-"completed" statuses (error, or a
/// partially-written trace from a killed process) are retried.
fn already_completed(trace_path: &std::path::Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(trace_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value.get("status").and_then(|s| s.as_str()) == Some("completed")
}
