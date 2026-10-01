//! `loop-goldloan-gen` — gold-loan workflow simulator. Human simulator vs six
//! staged tool-calling agents over a deterministic mock backend; writes one
//! trace JSON per scenario.
//!
//!   loop-goldloan-gen --scenario goldloan-datagen/scenarios/golden_demo.json --mode scripted
//!   loop-goldloan-gen --scenario <dir> --mode llm --agent-model soket/gpt --human-model soket/qwen3-8-27b

mod agent_llm;
mod agent_scripted;
mod backend;
mod human;
mod model_util;
mod orchestrator;
mod reply;
mod rules;
mod scenario;
mod stages;
mod tools;
mod trace;
mod vision;
mod docs;
mod langfuse;
mod lf_capture;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use loop_ai::providers::SOKET_PROVIDER_ID;
use loop_ai::ModelsRefreshOptions;
use loop_app_core::config::auth::FileCredentialStore;
use loop_app_core::config::paths::{auth_path, ensure_agent_dirs, get_agent_dir};
use loop_app_core::runtime::{build_models, ensure_provider_connected};

#[derive(Clone, Copy, ValueEnum, Debug)]
enum ModeArg {
    Scripted,
    Llm,
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum VisionArg {
    Auto,
    Deterministic,
    Llm,
}

#[derive(Debug, Parser)]
#[command(name = "loop-goldloan-gen", about = "Gold-loan workflow simulator and synthetic trace generator")]
struct Cli {
    /// Scenario JSON file(s), or directories of them. May be repeated / space separated.
    #[arg(long, num_args = 1.., required = true)]
    scenario: Vec<PathBuf>,
    /// Directory to write traces into.
    #[arg(long, default_value = "artifacts/goldloan_dataset/traces")]
    out: PathBuf,
    #[arg(long, value_enum, default_value = "scripted")]
    mode: ModeArg,
    #[arg(long, default_value = "soket/gpt")]
    agent_model: String,
    #[arg(long, default_value = "soket/gpt")]
    human_model: String,
    /// Model that plays the OCR / vision extractor.
    #[arg(long, default_value = "soket/gpt")]
    vision_model: String,
    /// auto = llm vision in llm mode, deterministic in scripted mode.
    #[arg(long, value_enum, default_value = "auto")]
    vision: VisionArg,
    /// In llm mode, still use the deterministic human policy (isolates agent quality).
    #[arg(long)]
    scripted_human: bool,
    /// Override business rules (default: embedded goldloan-datagen/spec/business_rules.json).
    #[arg(long)]
    rules: Option<PathBuf>,
    #[arg(long, default_value_t = 150)]
    max_turns: u32,
    /// Re-run scenarios whose trace already exists and completed.
    #[arg(long)]
    force: bool,
    /// Keep the per-run session databases (raw model messages) for debugging.
    #[arg(long)]
    keep_sessions: bool,
    /// Export every model call as a trace to Langfuse (needs LANGFUSE_PUBLIC_KEY /
    /// LANGFUSE_SECRET_KEY / LANGFUSE_BASE_URL, from goldloan-datagen/.env).
    #[arg(long)]
    langfuse: bool,
    /// Suffix for Langfuse session ids, so reruns of the same scenario+seed get a new session
    /// (default: current UTC time, e.g. 20260930T061500).
    #[arg(long)]
    run_id: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Project-scoped secrets, loaded explicitly (not the generic dotenv() every other
    // binary in this workspace calls) so they can never leak into another tool's process.
    // `from_filename_iter` (not `from_filename`) is deliberate: dotenvy's normal load only
    // fills in vars that are NOT already set in the process, so a shell that already has
    // e.g. LANGFUSE_PUBLIC_KEY exported from unrelated earlier work (a real, live case: it
    // silently kept routing every run to an old "llm logging" Langfuse project instead of
    // this project's .env) would win over this file every time with no error or warning.
    // Reading the file's own key=value pairs and force-setting them makes this project's
    // .env unconditionally authoritative for the vars it defines, regardless of whatever
    // the calling shell happens to already have.
    if let Ok(iter) = dotenvy::from_filename_iter("goldloan-datagen/.env") {
        for item in iter.flatten() {
            std::env::set_var(item.0, item.1);
        }
    }
    dotenvy::dotenv().ok();
    // Forces the Soket auth key for THIS process only: env_api_key_auth (loop-ai) checks
    // SOKET_API_KEY before the shared ~/.loop/agent/auth.json credential, so this overrides
    // it here without ever reading or writing that shared file.
    if let Ok(key) = std::env::var("GOLDLOAN_SOKET_API_KEY") {
        if !key.trim().is_empty() {
            std::env::set_var("SOKET_API_KEY", key);
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let rules = rules::Rules::load(cli.rules.as_deref())?;
    let mut scenarios = vec![];
    for p in &cli.scenario {
        scenarios.extend(scenario::load_scenarios(p)?);
    }
    let mode = match cli.mode {
        ModeArg::Scripted => orchestrator::Mode::Scripted,
        ModeArg::Llm => orchestrator::Mode::Llm,
    };

    let agent_dir = get_agent_dir();
    ensure_agent_dirs(&agent_dir)?;
    // One session directory per process: parallel workers must not share SQLite files.
    let state_dir = agent_dir.join("goldloan-gen-state").join(format!("run-{}", std::process::id()));
    std::fs::create_dir_all(&state_dir)?;

    let models = if mode == orchestrator::Mode::Llm {
        // loop-ai resolves a provider's key as: stored credential (auth.json) BEFORE the env var,
        // so a key saved in the shared ~/.loop/agent/auth.json silently beat GOLDLOAN_SOKET_API_KEY
        // (found live: runs kept sending a stale shared key and got 401s). With a project key
        // set, use this run's own empty store instead, so the env key always wins and the shared
        // file is never read or written.
        let project_key = std::env::var("GOLDLOAN_SOKET_API_KEY").is_ok_and(|k| !k.trim().is_empty());
        let auth_file = if project_key { state_dir.join("auth.json") } else { auth_path(&agent_dir) };
        let credentials = Arc::new(FileCredentialStore::open(auth_file)?);
        ensure_provider_connected(&credentials, &[], false)?;
        let models = build_models(&agent_dir, Arc::clone(&credentials), &[])?;
        let refresh = models
            .refresh(ModelsRefreshOptions { allow_network: Some(true), force: true, provider_id: Some(SOKET_PROVIDER_ID.into()) })
            .await;
        for (pid, err) in &refresh.errors {
            tracing::warn!("model refresh {pid}: {err}");
        }
        Some(models)
    } else {
        None
    };

    let vision_llm = match cli.vision {
        VisionArg::Auto => mode == orchestrator::Mode::Llm,
        VisionArg::Llm => true,
        VisionArg::Deterministic => false,
    };
    if vision_llm && models.is_none() {
        anyhow::bail!("--vision llm needs --mode llm (model access)");
    }
    let run_id = cli.run_id.clone().unwrap_or_else(|| chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string());
    let lf: langfuse::Langfuse = if cli.langfuse {
        Some(Arc::new(langfuse::LangfuseClient::from_env(&run_id).map_err(|e| {
            anyhow::anyhow!("--langfuse was passed but {e} (expected in goldloan-datagen/.env)")
        })?))
    } else {
        None
    };
    let opts = orchestrator::RunOpts {
        mode,
        max_turns: cli.max_turns,
        agent_model: cli.agent_model.clone(),
        human_model: cli.human_model.clone(),
        scripted_human: cli.scripted_human,
        vision_llm,
        vision_model: cli.vision_model.clone(),
        run_id,
    };

    println!("Loaded {} scenario(s)", scenarios.len());
    let (mut ok, mut failed, mut skipped) = (0, 0, 0);
    for sc in &scenarios {
        if let Err(e) = sc.validate(&rules) {
            println!("{} -> INVALID: {e}", sc.id);
            failed += 1;
            continue;
        }
        let path = cli.out.join(format!("{}.json", sc.id));
        if !cli.force && already_completed(&path) {
            skipped += 1;
            continue;
        }
        match orchestrator::run(sc, &rules, &opts, models.clone(), &state_dir, lf.clone()).await {
            Ok(trace) => {
                let p = trace.write_to(&cli.out)?;
                println!("{} -> {} ({} turns, outcome={}, status={})", sc.id, p.display(), trace.conversation.len(), trace.outcome, trace.status);
                if trace.status == "completed" { ok += 1 } else { failed += 1 }
            }
            Err(e) => {
                println!("{} -> ERROR: {e}", sc.id);
                failed += 1;
            }
        }
    }
    println!("=== {ok} completed, {failed} failed, {skipped} skipped (of {}) ===", scenarios.len());
    if !cli.keep_sessions {
        let _ = std::fs::remove_dir_all(&state_dir);
    }
    if lf.is_some() {
        // Langfuse sends are fire-and-forget spawned tasks, each retrying up to 3x with
        // backoff (worst case ~40s) against a possibly slow/overloaded instance; give the
        // last few a real chance to land before the tokio runtime (and this process) shuts down.
        tokio::time::sleep(std::time::Duration::from_secs(45)).await;
    }
    Ok(())
}

fn already_completed(p: &std::path::Path) -> bool {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
        .map(|v| v.get("status").and_then(|s| s.as_str()) == Some("completed"))
        .unwrap_or(false)
}
