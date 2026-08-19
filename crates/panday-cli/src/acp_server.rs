//! `panday acp` — the ACP bridge over stdio (M16.5, docs/16 §ACP bridge, ADR-012).
//!
//! > "`panday acp` (in panday-cli, ADR-012) speaks Agent Client Protocol v1 over stdio
//! > using the official `agent-client-protocol` crate. ... Thirteen editors (Zed,
//! > JetBrains, VS Code, nvim, Emacs…) become clients for the cost of one adapter — the
//! > best distribution-per-line-of-code in the plan."
//!
//! The mapping itself is M3.4 (`crate::acp`); this is the transport and the turn
//! choreography around it.
//!
//! ## The permission round trip is the hard part
//!
//! Everything else here is a request/response or a notification. A permission gate is
//! neither: the agent must stop mid-turn, ask the *client* a question, and resume with
//! the answer. That inverts the usual direction — the agent becomes the caller — and it
//! is why `session_update` deliberately has no arm for `PermissionRequest` (M3.4): an
//! update is fire-and-forget, and a tool would run before anyone answered.
//!
//! ## Why the session lives here and not in `panday-harnessd`
//!
//! ADR-012 puts the ACP server in the CLI, on the user's machine, holding the loop
//! in-process. That is the same reason `panday local` exists: an editor session on a
//! laptop should not require a hosted service, and the harness is a library precisely
//! so it can run either way.

use crate::acp as map;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionResponse, SessionNotification, StopReason as AcpStopReason,
};
use agent_client_protocol::{Agent, ConnectTo, ConnectionTo};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    CollectSink, EventSink, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget,
    TurnOutcome,
};
use panday_sdk::ModelClient;
use panday_types::event::{Actor, Envelope};
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Everything the bridge needs to build a session. A struct rather than positional
/// arguments because the CLI and the test suite wire it differently, and a five-argument
/// `serve()` invites getting two of them the wrong way round.
pub struct AcpDeps {
    pub model: Arc<dyn ModelClient>,
    pub model_ref: ModelRef,
    /// Called per session: tools are per-session state (a registry holds
    /// `Box<dyn Tool>`), and sharing one across sessions would share a workspace.
    pub tools: Arc<dyn Fn() -> ToolRegistry + Send + Sync>,
    pub profile: Profile,
    pub account: AccountId,
}

/// Sessions the client has opened.
#[derive(Default)]
struct Sessions {
    actors: Mutex<HashMap<String, Arc<tokio::sync::Mutex<SessionActor>>>>,
}

/// Forwards every event the actor commits into a channel, so the turn loop can
/// translate and send them as `session/update` notifications.
///
/// A channel rather than sending directly from the sink: `EventSink::emit` is
/// synchronous (docs/13 — a sink that can await is a sink that can stall a turn), and
/// sending on an ACP connection is async.
struct Forward(tokio::sync::mpsc::UnboundedSender<Envelope>);

impl EventSink for Forward {
    fn emit(&self, envelope: &Envelope) {
        // A closed receiver means the client hung up mid-turn; the log still has
        // everything, so dropping the notification is the whole cost.
        let _ = self.0.send(envelope.clone());
    }
}

/// Serve ACP on stdio until the client disconnects.
pub async fn serve_stdio(deps: AcpDeps) -> Result<(), String> {
    serve(deps, agent_client_protocol::Stdio::new()).await
}

/// Serve ACP over any transport. The test suite passes an in-memory `Channel`, which is
/// how the round trip below is exercised without spawning a process.
pub async fn serve<T>(deps: AcpDeps, transport: T) -> Result<(), String>
where
    T: ConnectTo<Agent>,
{
    let deps = Arc::new(deps);
    let sessions = Arc::new(Sessions::default());

    let for_new = (deps.clone(), sessions.clone());
    let for_prompt = (deps.clone(), sessions.clone());

    Agent
        .builder()
        .name("panday")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                // Echo the client's version: docs/16 pins v1, and answering with a
                // version the client did not offer is how a handshake fails obscurely.
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _connection| {
                let (deps, sessions) = &for_new;
                let id = SessionId::new();
                let actor = build_actor(deps, id);
                sessions
                    .actors
                    .lock()
                    .unwrap()
                    .insert(id.0.to_string(), Arc::new(tokio::sync::Mutex::new(actor)));
                responder.respond(NewSessionResponse::new(
                    agent_client_protocol::schema::v1::SessionId::new(id.0.to_string()),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let (_deps, sessions) = &for_prompt;
                let actor = sessions
                    .actors
                    .lock()
                    .unwrap()
                    .get(request.session_id.0.as_ref())
                    .cloned();
                let Some(actor) = actor else {
                    // A prompt for a session we never opened is a client bug, and an
                    // error is kinder than inventing a session for it.
                    return responder.respond(PromptResponse::new(AcpStopReason::Refusal));
                };

                let text = prompt_text(&request.prompt);
                let session_id = request.session_id.clone();
                // Spawned, not awaited here. A turn asks the *client* for permission
                // mid-way, and awaiting that from inside a request handler deadlocks:
                // the handler blocks the dispatch loop, so the loop cannot deliver the
                // answer the handler is waiting for. The crate documents this
                // (`block_task`'s "❌ DEADLOCK" example) and the first draft did it
                // anyway — the whole ACP suite hung. Responding from a spawned task
                // frees the loop; `Responder` is `Send` and `respond` consumes it,
                // which is exactly the shape a late response needs.
                tokio::spawn(async move {
                    let stop = run_turn(actor, text, connection, session_id).await;
                    let _ = responder.respond(PromptResponse::new(stop));
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| format!("acp: {e}"))
}

fn build_actor(deps: &AcpDeps, session: SessionId) -> SessionActor {
    SessionActor::new(
        session,
        deps.account,
        deps.model_ref.clone(),
        Arc::new(MemoryStore::new()),
        deps.model.clone(),
        (deps.tools)(),
        PermissionEngine::new(deps.profile),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    )
}

/// ACP sends a prompt as content blocks; the harness takes text.
fn prompt_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(text) => Some(text.text.clone()),
            // An editor may send an image or a resource; the harness takes text, and
            // saying nothing about the rest is better than pretending it arrived.
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One turn, including any number of permission round trips.
async fn run_turn(
    actor: Arc<tokio::sync::Mutex<SessionActor>>,
    text: String,
    connection: ConnectionTo<agent_client_protocol::Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
) -> AcpStopReason {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut actor = actor.lock().await;
    actor.subscribe(Arc::new(Forward(tx)));

    let mut outcome: TurnOutcome = match actor.handle_user_input(&text).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, "acp turn failed");
            return AcpStopReason::Refusal;
        }
    };

    loop {
        // Drain the events this step produced *before* asking anything: an editor that
        // is asked to approve `bash` should already be showing the tool call it belongs
        // to, and the permission request references it by `toolCallId`.
        drain(&mut rx, &connection, &session_id).await;

        match outcome.clone() {
            TurnOutcome::Finished(reason) => {
                return match reason {
                    StopReason::EndTurn | StopReason::StopSequence => AcpStopReason::EndTurn,
                    StopReason::MaxTokens => AcpStopReason::MaxTokens,
                    // ACP has no "budget" reason; `max_turn_requests` is the closest
                    // honest mapping — both mean "the agent stopped itself".
                    StopReason::MaxSteps | StopReason::BudgetExceeded => {
                        AcpStopReason::MaxTurnRequests
                    }
                    StopReason::Cancelled => AcpStopReason::Cancelled,
                    StopReason::Error => AcpStopReason::Refusal,
                    // A turn that ended in a tool call has not ended; treat it as a
                    // refusal rather than reporting success on an unfinished turn.
                    StopReason::ToolUse => AcpStopReason::Refusal,
                };
            }
            TurnOutcome::AwaitingPermission(ids) => {
                for call_id in ids {
                    let Some(request) = pending_request(&actor, call_id, &session_id) else {
                        continue;
                    };
                    let answer = connection.send_request(request).block_task().await;
                    let decision = match answer {
                        Ok(RequestPermissionResponse {
                            outcome: RequestPermissionOutcome::Selected(selected),
                            ..
                        }) => {
                            map::decision_of(selected.option_id.0.as_ref()).map(|(d, _remember)| d)
                        }
                        // Cancelled, or the client failed to answer at all. Denying is
                        // the only safe reading: a gate whose answer got lost must not
                        // default to running the tool.
                        _ => Some(panday_types::event::PermDecision::Deny),
                    };
                    let Some(decision) = decision else {
                        continue;
                    };
                    outcome = match actor.decide(call_id, decision, Actor::User).await {
                        Ok(o) => o,
                        Err(e) => {
                            tracing::warn!(error = %e, "acp decision failed");
                            return AcpStopReason::Refusal;
                        }
                    };
                }
            }
        }
    }
}

/// Rebuild the ACP permission request for a parked call from the log.
fn pending_request(
    actor: &SessionActor,
    call_id: panday_types::CallId,
    session_id: &agent_client_protocol::schema::v1::SessionId,
) -> Option<agent_client_protocol::schema::v1::RequestPermissionRequest> {
    let event = actor.parked_permission(call_id)?;
    map::permission_request(session_id, &event)
}

/// Send every buffered event as a `session/update`.
async fn drain(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Envelope>,
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
) {
    while let Ok(envelope) = rx.try_recv() {
        if let Some(update) = map::session_update(&envelope.event) {
            // Notifications are fire-and-forget by design: an editor that is not
            // listening must not be able to stall a turn.
            let _ =
                connection.send_notification(SessionNotification::new(session_id.clone(), update));
        }
    }
}

/// Discards events. Used when a caller wants the loop without the stream.
pub fn no_sink() -> Arc<CollectSink> {
    Arc::new(CollectSink::new())
}
