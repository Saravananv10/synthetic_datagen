//! `AgentService.Chat` — one bidi stream per turn (see `registry.rs` for the
//! session-persists-across-streams design this implements).

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::pb;
use crate::registry::ConversationRegistry;

pub struct AgentServiceImpl {
    pub registry: Arc<ConversationRegistry>,
    /// When true, `bash` calls never pause for `ActionRequired` — the
    /// intended mode for unattended `python -m datagen` runs.
    pub auto_approve: bool,
}

#[tonic::async_trait]
impl pb::agent_service_server::AgentService for AgentServiceImpl {
    type ChatStream = Pin<Box<dyn Stream<Item = Result<pb::ServerMessage, Status>> + Send>>;

    async fn chat(
        &self,
        request: Request<Streaming<pb::ClientMessage>>,
    ) -> Result<Response<Self::ChatStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel::<Result<pb::ServerMessage, Status>>(64);
        let registry = Arc::clone(&self.registry);
        let auto_approve = self.auto_approve;

        tokio::spawn(async move {
            let first = match inbound.message().await {
                Ok(Some(msg)) => msg,
                _ => return,
            };
            let req = match first.payload {
                Some(pb::client_message::Payload::Request(req)) => req,
                _ => {
                    let _ = tx
                        .send(Err(Status::invalid_argument(
                            "first message on the stream must be a ChatRequest",
                        )))
                        .await;
                    return;
                }
            };

            let cwd = if req.working_directory.is_empty() {
                ".".to_string()
            } else {
                req.working_directory.clone()
            };
            let session_id = if req.session_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                req.session_id.clone()
            };

            let conversation = match registry.get_or_create(&session_id, &cwd).await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                    return;
                }
            };

            let model_override = req
                .model
                .as_deref()
                .and_then(|spec| registry.resolve_model(spec));

            let conv_for_turn = Arc::clone(&conversation);
            let out_tx = tx.clone();
            let mut turn_handle = tokio::spawn(async move {
                conv_for_turn
                    .run_turn(req.message, model_override, out_tx, auto_approve)
                    .await;
            });

            let mut inbound_open = true;
            loop {
                if !inbound_open {
                    let _ = (&mut turn_handle).await;
                    break;
                }
                tokio::select! {
                    biased;
                    res = &mut turn_handle => {
                        let _ = res;
                        break;
                    }
                    maybe_msg = inbound.message() => {
                        match maybe_msg {
                            Ok(Some(pb::ClientMessage { payload: Some(pb::client_message::Payload::Input(input)) })) => {
                                conversation.resolve_user_input(&input.prompt_id, input.reply);
                            }
                            Ok(Some(pb::ClientMessage { payload: Some(pb::client_message::Payload::Cancel(_)) })) => {
                                conversation.cancel();
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => {
                                inbound_open = false;
                            }
                        }
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
