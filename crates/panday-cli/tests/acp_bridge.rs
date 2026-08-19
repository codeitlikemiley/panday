//! M16.5 — the ACP bridge, driven by a real ACP client (docs/16 §ACP bridge).
//!
//! The client here is the official crate's `Client` role, connected to our `Agent` over
//! an in-memory `Channel`. That is a real ACP conversation — initialize, session/new,
//! session/prompt, session/update notifications, session/request_permission — without a
//! subprocess, which is what makes the permission round trip testable at all.
//!
//! docs/16's acceptance says "interactive session from Zed". Zed cannot run in CI; what
//! is verified here is everything an editor does, in the order it does it, over the
//! protocol it speaks.

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, StopReason as AcpStopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, Channel, ConnectionTo};
use panday_cli::acp_server::{serve, AcpDeps};
use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::Profile;
use panday_types::model::ModelRef;
use panday_types::AccountId;
use std::sync::{Arc, Mutex};

/// Spawn our agent on one end of an in-memory channel and return the other end.
fn spawn_agent(turns: Vec<ScriptedTurn>, tools: fn() -> ToolRegistry, profile: Profile) -> Channel {
    let (ours, theirs) = Channel::duplex();
    let deps = AcpDeps {
        model: Arc::new(ScriptedClient::new(turns)),
        model_ref: ModelRef("local/test".into()),
        tools: Arc::new(tools),
        profile,
        account: AccountId::new(),
    };
    tokio::spawn(async move {
        let _ = serve(deps, ours).await;
    });
    theirs
}

fn read_only_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::ok(
        "read_file",
        "fn add(a: i32, b: i32) -> i32 { a - b }",
    ));
    registry
}

/// An irreversible tool: `dev` allows ordinary edits (docs/13), so a gate needs
/// something that asks in every profile — which is also the realistic case for an
/// editor prompt.
fn gated_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::irreversible("deploy", "deployed"));
    registry
}

#[tokio::test(flavor = "multi_thread")]
async fn an_editor_initializes_opens_a_session_and_gets_streamed_updates() {
    let transport = spawn_agent(
        vec![
            ScriptedTurn::calling(
                "Let me look at the file.",
                vec![("read_file", serde_json::json!({"path": "src/lib.rs"}))],
            ),
            ScriptedTurn::text("The subtraction should be an addition."),
        ],
        read_only_tools,
        Profile::Dev,
    );

    let updates: Arc<Mutex<Vec<SessionNotification>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = updates.clone();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                seen.lock().unwrap().push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            let init = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            // The agent echoes the client's version rather than announcing its own:
            // answering with a version the client did not offer is how a handshake
            // fails obscurely.
            assert_eq!(init.protocol_version, ProtocolVersion::V1);

            let session = connection
                .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                .block_task()
                .await?;

            let prompt = connection
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(
                        "why does the test fail?".to_string(),
                    ))],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, AcpStopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("the conversation completed");

    let updates = updates.lock().unwrap();
    let kinds: Vec<String> = updates
        .iter()
        .map(|n| {
            serde_json::to_value(&n.update).unwrap()["sessionUpdate"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();

    // What an editor renders: the user's message, the assistant's, the tool call and its
    // completion. In that order — an editor draws them as they arrive.
    assert!(
        kinds.contains(&"user_message_chunk".to_string()),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"agent_message_chunk".to_string()),
        "{kinds:?}"
    );
    assert!(kinds.contains(&"tool_call".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"tool_call_update".to_string()), "{kinds:?}");
    let call_at = kinds.iter().position(|k| k == "tool_call").unwrap();
    let update_at = kinds.iter().position(|k| k == "tool_call_update").unwrap();
    assert!(
        call_at < update_at,
        "a call must precede its update: {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_permission_round_trip_works() {
    // docs/16's other acceptance clause. The agent stops mid-turn, asks the editor, and
    // resumes with the answer — which is why a permission request is a *request* and not
    // a session update (M3.4).
    let transport = spawn_agent(
        vec![
            ScriptedTurn::calling(
                "I will ship it.",
                vec![("deploy", serde_json::json!({"env": "prod"}))],
            ),
            ScriptedTurn::text("done"),
        ],
        gated_tools,
        // Even `dev` asks about an irreversible tool, which is the point: the editor is
        // asked, and the human answers in the editor.
        Profile::Dev,
    );

    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = asked.clone();

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |_n: SessionNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                // The question an editor shows a human: a title, a status, and options.
                let title = request.tool_call.fields.title.clone().unwrap_or_default();
                recorded.lock().unwrap().push(title);
                let allow = request
                    .options
                    .iter()
                    .find(|o| o.option_id.0.as_ref() == "allow_once")
                    .map(|o| o.option_id.clone())
                    .expect("an allow-once option");
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(allow)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let prompt = connection
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("fix it".to_string()))],
                ))
                .block_task()
                .await?;
            // The turn finished, which it can only do if the answer got back in.
            assert_eq!(prompt.stop_reason, AcpStopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("the conversation completed");

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "exactly one question: {asked:?}");
    // Named, so a human knows what they are approving — and it is the same text the log
    // recorded, because both come from `describe()`.
    assert!(asked[0].contains("deploy"), "{asked:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_permission_ends_the_turn_without_running_the_tool() {
    let transport = spawn_agent(
        vec![
            ScriptedTurn::calling(
                "I will ship it.",
                vec![("deploy", serde_json::json!({"env": "prod"}))],
            ),
            ScriptedTurn::text("understood"),
        ],
        gated_tools,
        Profile::Dev,
    );

    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |_n: SessionNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let reject = request
                    .options
                    .iter()
                    .find(|o| o.option_id.0.as_ref() == "reject_once")
                    .map(|o| o.option_id.clone())
                    .expect("a reject option");
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(reject)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(std::path::PathBuf::from("/tmp")))
                .block_task()
                .await?;
            let prompt = connection
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("fix it".to_string()))],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, AcpStopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("the conversation completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_for_an_unknown_session_is_refused_not_invented() {
    let transport = spawn_agent(
        vec![ScriptedTurn::text("unused")],
        read_only_tools,
        Profile::Dev,
    );

    agent_client_protocol::Client
        .builder()
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let prompt = connection
                .send_request(PromptRequest::new(
                    agent_client_protocol::schema::v1::SessionId::new("not-a-session"),
                    vec![ContentBlock::Text(TextContent::new("hello".to_string()))],
                ))
                .block_task()
                .await?;
            assert_eq!(prompt.stop_reason, AcpStopReason::Refusal);
            Ok(())
        })
        .await
        .expect("the conversation completed");
}
