//! gRPC tool-approval bridge: `bash` calls on the *coordinator* harness pause
//! and emit `ActionRequired`; the client's `UserInput{reply, prompt_id}`
//! resumes them. Patterned directly off
//! `loop_app_core::tool_approval::ToolApprovalBridge`'s oneshot-based
//! pause/resume, with the TUI's `mpsc::UnboundedSender<ApprovalPrompt>`
//! replaced by the gRPC response stream.
//!
//! Only the coordinator harness gets this bridge — subagent harnesses run
//! fully autonomously (see `subagents.rs`), so there is never a nested
//! approval chain to reason about.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use loop_agent::{BeforeToolCallContext, BeforeToolCallResult};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::pb;

/// Tools that pause for approval on the coordinator harness. Matches
/// `loop_app_core::tool_approval::interactive_tools()`'s bash-only wiring
/// for `before_tool_call` (file-edit review has no equivalent `ActionType`
/// on the wire contract, and datagen relies on the subagent design being
/// fully autonomous rather than on human-reviewed edits).
const APPROVED_TOOLS: &[&str] = &["bash"];

pub struct GrpcApprovalBridge {
    out: mpsc::Sender<Result<pb::ServerMessage, tonic::Status>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    /// When true, `bash` calls are approved without ever prompting — used
    /// for fully unattended datagen runs where no client is listening for
    /// `ActionRequired`.
    auto_approve: bool,
}

impl GrpcApprovalBridge {
    pub fn new(
        out: mpsc::Sender<Result<pb::ServerMessage, tonic::Status>>,
        auto_approve: bool,
    ) -> Self {
        Self {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            auto_approve,
        }
    }

    /// Resolve a pending prompt when the client sends `UserInput`.
    pub fn resolve(&self, prompt_id: &str, reply: String) {
        if let Some(tx) = self.pending.lock().remove(prompt_id) {
            let _ = tx.send(reply);
        }
    }

    /// Hook for [`loop_agent::harness::AgentHarness::set_before_tool_call`].
    pub fn before_tool_hook(
        self: &Arc<Self>,
    ) -> Arc<
        dyn Fn(
                BeforeToolCallContext,
                Option<CancellationToken>,
            ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
            + Send
            + Sync,
    > {
        let bridge = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let bridge = Arc::clone(&bridge);
            Box::pin(async move { bridge.review(ctx, cancel).await })
        })
    }

    async fn review(
        &self,
        ctx: BeforeToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> Option<BeforeToolCallResult> {
        let name = ctx.tool_call.name.as_str();
        if !APPROVED_TOOLS.contains(&name) || self.auto_approve {
            return None;
        }

        let command = ctx
            .args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let prompt_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(prompt_id.clone(), tx);

        let sent = self
            .out
            .send(Ok(pb::ServerMessage {
                event: Some(pb::server_message::Event::ActionRequired(
                    pb::ActionRequired {
                        prompt_id: prompt_id.clone(),
                        question: format!("Run command: {command}"),
                        r#type: pb::action_required::ActionType::ConfirmCommand as i32,
                        tool_use_id: ctx.tool_call.id.clone(),
                    },
                )),
            }))
            .await;
        if sent.is_err() {
            return Some(BeforeToolCallResult {
                block: true,
                reason: Some("approval stream closed".into()),
            });
        }

        let reply = tokio::select! {
            biased;
            _ = async {
                if let Some(c) = &cancel {
                    c.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => None,
            res = rx => res.ok(),
        };

        match reply.as_deref() {
            Some("yes") | Some("y") => None,
            _ => Some(BeforeToolCallResult {
                block: true,
                reason: Some("command not approved".into()),
            }),
        }
    }
}
