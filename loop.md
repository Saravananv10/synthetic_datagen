Loop — Codebase Overview
Loop (by Soket AI) is a Rust workspace implementing an AI coding agent, similar in spirit to Claude Code: a unified LLM client, a stateful agent/tool-execution engine, and two front-ends (a ratatui TUI and a GPUI desktop app). Workspace root: Cargo.toml, 7 crates under crates/.

Architecture at a glance

loop-ai            ← LLM provider abstraction (the "model" layer)
   ↑
loop-mcp           ← MCP client/server (external tool protocol)
loop-orchestration ← optional multi-agent workflow engine
   ↑
loop-agent         ← agent loop, tools, sessions, sandboxing (the "brain")
   ↑
loop-app-core      ← shared config/bootstrap glue for front-ends
   ↑            ↑
loop-cli      loop-desktop   ← TUI and GPUI front-ends (the "faces")

1. loop-ai — Unified LLM API
   crates/loop-ai/src/lib.rs: "Unified LLM API with provider collections, auth resolution, token/cost tracking, and mid-session model hand-off."

providers/ — pluggable backends, all speaking a common Provider trait:
soket.rs — the built-in Soket/TensorStudio provider; fetches its model catalog dynamically from /v1/models and caches it via models_store.rs. Default provider/model ids (SOKET_PROVIDER_ID, SOKET_DEFAULT_MODEL_ID = "qwen3-30b") live here.
custom.rs — user-defined OpenAI-compatible endpoints (e.g. Ollama, local servers).
faux.rs — scripted fake provider for tests.
api/ — wire-format handling: openai_completions.rs (streaming chat completions), openai_models.rs (model listing), detect_compat.rs (auto-detects API dialect quirks per endpoint), transform_messages.rs, http.rs.
auth/ — Credential/CredentialStore trait plus env-var and resolution helpers (auth/resolve.rs); the concrete file-backed store lives up in loop-app-core.
models.rs / models_store.rs — the Models registry (holds all configured providers/models at runtime) and a persistent JSON cache for dynamically refreshed catalogs.
types.rs — the core shared vocabulary: Message, Context, AssistantMessage, Tool, ToolCall, streaming AssistantMessageEvent, thinking-level mapping, usage/cost types.
utils/ — cost calculation (cost.rs, Anthropic-style cache-write pricing awareness), token estimation, context-overflow detection, partial-JSON streaming parser (for incremental tool-call argument decoding).
stream/ — the streaming event abstraction returned by models.stream_simple(...).
This crate has no notion of "agent" or "tools execution" — it's a thin, provider-agnostic chat/completions client, analogous to a Rust version of the Anthropic/OpenAI SDKs unified behind one interface.

2. loop-mcp — Model Context Protocol
   crates/loop-mcp/src/lib.rs: client + server for MCP.

McpClientManager (client.rs) connects to external MCP servers over stdio or streamable HTTP — this is how Loop consumes third-party MCP tool servers.
McpServer (server.rs) exposes Loop's own tools over streamable HTTP via a ToolProvider trait, so Loop itself can act as an MCP server (used by loop-cli/src/mcp_serve.rs).
3. loop-orchestration — multi-agent workflows (optional plugin)
crates/loop-orchestration/src/lib.rs: "durable workflows, task planning, scheduling, and shared memory." Designed to plug into loop-agent via abstract traits (AgentRunner, ShellRunner) to avoid a circular dependency.

workflow/ — a durable execution engine: engine.rs, checkpoint.rs, an event log persisted to SQLite (sqlite_event_log.rs), signals.
planner/ — turns a goal into a task_graph.rs; either manual_planner.rs or llm_planner.rs (LLM-generated plan).
scheduler/ — a worker pool (pool.rs, worker.rs) that executes the task graph, producing artifacts.rs.
memory/ — shared/task memory and an event bus.rs agents can read/write, exposed back into loop-agent as tools (see below).
4. loop-agent — the agent engine
crates/loop-agent/src/lib.rs: "Stateful agent with tool execution and event streaming." Built directly on loop_ai. Two layers:

Low level — agent_loop.rs: a single-turn loop — send context to the model, stream response, execute any tool calls, feed results back, repeat until a stop condition. agent.rs wraps this in a stateful Agent with queues/subscribers for event-driven consumption. stream_fn.rs resolves which streaming function to call per model/provider.

Harness (production layer) — harness/agent_harness.rs: "sessions, turn snapshots, tools, sandbox." This is the real orchestration surface the CLI/desktop drive:

tools/mod.rs — built-in tools: create_read_tool, create_write_tool, create_edit_tool (with fuzzy replace + unified diff + review snapshots), create_bash_tool (with check_command_policy for blocked commands). These operate through an ExecutionEnv trait (harness/env/), abstracting local vs. sandboxed execution.
sandbox/ — pluggable execution backends: krun.rs (microVM), podman.rs (container), plus registry.rs/traits.rs. This is Loop's sandboxing story for running bash/file tools isolated from the host.
session/ — session persistence: a session tree (supports forking, fork.rs), stored either as JSONL (jsonl_store.rs) or SQLite (session/sqlite/, with materialization, branch caching, and full-text search).
compaction/ — auto-summarizes/truncates context when it grows too large, with branch-aware summarization.
mcp/bridge.rs — adapts MCP tools into the harness's native AgentTool type so MCP-provided tools are indistinguishable from built-ins.
orchestration/ — the adapter wiring loop-orchestration into the harness: AgentWorker/SubWorkflowWorker (run sub-agents/sub-workflows as scheduler tasks) and memory_read/memory_write/memory_list tools exposed to the LLM.
skills.rs — loads SKILL.md files (same concept as Claude Code skills).
hooks.rs — a typed registry for lifecycle hooks (pre/post tool call, etc.), user-configurable.
system_prompt.rs / prompt_templates.rs — system prompt assembly and template formatting.
5. loop-app-core — shared bootstrap/config
crates/loop-app-core/src/lib.rs: "Shared bootstrap, configuration, and runtime wiring for Loop CLI and desktop" — this is the glue both front-ends depend on so they don't duplicate setup logic.

runtime.rs — top-level bootstrap(): resolves the agent dir, loads settings/credentials, ensures the Soket API key exists (prompts if missing), constructs the Models registry and wires the default model/provider.
config/ — settings.rs (the Settings struct — default model/provider/theme/sandbox/etc., loaded from settings.json, project-overlaid), auth.rs (FileCredentialStore → auth.json, 0600 permissions), paths.rs (~/.loop/agent/... resolution, $LOOP_CODING_AGENT_DIR override), trust.rs (per-project trust prompts).
extensions/ — a Rhai scripting host implementing a "pi ExtensionAPI subset" — i.e., user-authored extensions in Rhai script.
resources/mod.rs — discovers skills, prompts, context files (CLAUDE.md/AGENTS.md-equivalents), extensions, hooks, themes from the filesystem.
theme/ — pi-compatible JSON theme loading/color resolution (shared visual theming between TUI and desktop).
tool_approval.rs — the interactive "ask/allow/deny" approval flow for file edits and bash commands, with session-level auto-approve memory.
6. loop-cli — the TUI front-end
crates/loop-cli/src/main.rs: clap-based binary, entry point loop.

app.rs — the large central controller: command dispatch (/model, /login, /skills, etc.), state management, wiring harness events to UI.
commands/mod.rs — slash-command definitions.
tui/ — ratatui rendering: editor.rs (input box), markdown.rs + highlight.rs (syntect-based rendering), file_mentions.rs (@file autocomplete), history.rs.
mcp_serve.rs — runs Loop itself as an MCP server (loop-mcp's McpServer).
keybindings.rs, clipboard.rs.
7. loop-desktop — the GPUI desktop app
crates/loop-desktop/src/main.rs, built on Zed's gpui framework (per root Cargo.toml:71-74).

app.rs — root application shell.
controller.rs — desktop analogue of loop-cli/app.rs, driving the harness.
chat_ui.rs, markdown.rs — chat rendering.
state/ — chat.rs, diff.rs (file-edit diff view state), stats.rs (usage/cost stats).
approval.rs — GUI equivalent of tool_approval.rs.
editor_launcher.rs, session_title.rs.
End-to-end flow (a typical turn)
Bootstrap (loop-app-core::runtime) loads settings.json + auth.json, builds a Models registry (loop-ai), ensures an API key exists.
User input reaches agent_harness.rs, which loads/creates a session, assembles a system prompt + context, and calls into agent_loop.rs.
agent_loop streams a completion from the selected provider (loop-ai), emitting AgentEvents.
If the model calls a tool (read/write/edit/bash/MCP tool/memory tool), it executes through ExecutionEnv (optionally sandboxed via krun/podman), subject to tool_approval gating, and the result is fed back for another turn.
Session state is persisted (JSONL or SQLite), with compaction kicking in if the context grows too large.
Events flow up to either loop-cli (ratatui) or loop-desktop (gpui) for rendering.
Optionally, loop-orchestration can drive multi-agent workflows where each task is executed by an AgentWorker wrapping this same loop.
Where to look for specific things:

Change default model → crates/loop-ai/src/providers/soket.rs / user settings.json
Set API key → ~/.loop/agent/auth.json or SOKET_API_KEY env, via config/auth.rs
Add a built-in tool → harness/tools/mod.rs
Add a slash command → loop-cli/src/commands/mod.rs + app.rs
Sandbox behavior → harness/sandbox/
Session storage format → harness/session/
