//! AEP ⇄ ACP mapping (M3.4; docs/03 §"The CLI/harness maps AEP⇄ACP", docs/16
//! §ACP bridge).
//!
//! docs/16 calls the mapping "mechanical", which is true of the shape and not of
//! the seams. Three of them decide whether an editor shows a coherent session:
//!
//! 1. **A tool call is one ACP entity across two AEP events.** `ToolCall` and
//!    `ToolResult` are separate log entries; ACP models them as one `toolCallId`
//!    that starts `in_progress` and is *updated* to `completed`/`failed`. Emitting
//!    two `ToolCall` updates would make an editor draw the same call twice.
//! 2. **A permission request is not a session update.** It is a request *to* the
//!    client (`session/request_permission`) whose answer the agent waits for, so
//!    it maps to a different type entirely (`RequestPermissionRequest`) and is
//!    kept out of `session_update`.
//! 3. **Unknown events map to nothing, not to an error** (docs/03 §versioning).
//!    A newer server talking to this bridge must degrade to "some updates I do
//!    not render", never "the editor dropped the session".
//!
//! This is the table only. The stdio transport, the `session/new` handshake and
//! the round trip of an answer belong to M16.5; writing the table against the
//! official crate's types now is what keeps that milestone to transport work
//! rather than a second translation layer.

// docs/16 pins ACP **v1**; the crate also carries a v2 draft behind a feature.
// Importing the versioned module rather than the crate root makes that pin
// explicit — a silent move to the draft would change the wire format.
use agent_client_protocol::schema::v1 as acp;
use panday_types::event::{Actor, Envelope, Event, PermDecision, ReducedOutput};
use panday_types::model::ContentBlock;

/// Text carried by a block, for the surfaces ACP renders as prose.
fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolOutput { text, .. } => text.clone(),
            // An artifact's body is deliberately not inlined: it was spilled out
            // of context on purpose (docs/15), and pasting it into an editor pane
            // would undo that at the one place a human is watching.
            ContentBlock::Artifact { summary, .. } => summary.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn chunk(text: String) -> acp::ContentChunk {
    acp::ContentChunk::new(acp::ContentBlock::from(text))
}

/// Which icon and affordance an editor should use for one of our tools.
///
/// Our native set is fixed (docs/13), so this is a table and not a guess; an
/// unrecognised tool — an MCP tool, a plugin tool — is `Other`, which is what
/// ACP's own default is for exactly this reason.
pub fn tool_kind(tool: &str) -> acp::ToolKind {
    match tool {
        "read_file" => acp::ToolKind::Read,
        "write_file" | "edit_file" => acp::ToolKind::Edit,
        "grep" | "glob" => acp::ToolKind::Search,
        "bash" => acp::ToolKind::Execute,
        "spawn_subagent" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// A human-readable one-liner for a tool call, which is what an editor shows in
/// its collapsed row.
fn title(tool: &str, args: &serde_json::Value) -> String {
    // Prefer the argument that says *what* the call is about; falling back to the
    // whole JSON blob makes the row unreadable, and showing nothing makes every
    // row identical.
    for key in ["path", "pattern", "cmd", "command", "file"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return format!("{tool}: {v}");
        }
    }
    tool.to_string()
}

/// The core six, plus the two that a replay into an editor needs.
///
/// `None` means "this event has no ACP equivalent" — a turn boundary, a
/// compaction, an event from a newer version. Never an error: see the module
/// note.
pub fn session_update(event: &Event) -> Option<acp::SessionUpdate> {
    Some(match event {
        Event::UserMessage { content, .. } => {
            acp::SessionUpdate::UserMessageChunk(chunk(text_of(content)))
        }
        // Deltas are where ACP earns its keep: the editor renders them as they
        // arrive. They are never persisted (docs/03), so this arm only fires on
        // a live stream, never on a replay.
        Event::AssistantDelta { text } => {
            acp::SessionUpdate::AgentMessageChunk(chunk(text.clone()))
        }
        // The folded message. On a live session the deltas already said this, so
        // a bridge sends one or the other — see `live_updates` / `replay_updates`.
        Event::AssistantMessage { content, .. } => {
            acp::SessionUpdate::AgentMessageChunk(chunk(text_of(content)))
        }
        Event::ToolCall {
            call_id,
            tool,
            args,
            ..
        } => acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(call_id_of(call_id), title(tool, args))
                .kind(tool_kind(tool))
                // `InProgress`, not `Pending`: by the time this event is in the
                // log the gate has already passed and the tool is running.
                // `Pending` in ACP means "awaiting approval", which is the
                // permission flow's job.
                .status(acp::ToolCallStatus::InProgress)
                .locations(locations(args))
                .raw_input(args.clone()),
        ),
        Event::ToolResult {
            call_id,
            output,
            is_error,
            ..
        } => acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            call_id_of(call_id),
            acp::ToolCallUpdateFields::new()
                .status(if *is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                })
                // The *reduced* text, not the raw output: the reduction is what
                // the model saw, and an editor showing something else would be
                // debugging a different session (docs/15).
                .content(vec![acp::ToolCallContent::from(reduced_text(output))]),
        )),
        _ => return None,
    })
}

/// Tool arguments that name a file let an editor follow along.
fn locations(args: &serde_json::Value) -> Vec<acp::ToolCallLocation> {
    args.get("path")
        .and_then(|v| v.as_str())
        .map(|p| vec![acp::ToolCallLocation::new(p)])
        .unwrap_or_default()
}

/// AEP `CallId` → ACP `ToolCallId`. One function so the two events that describe
/// the same call cannot disagree about its id — which would make an editor draw
/// a second row instead of completing the first.
fn call_id_of(id: &panday_types::CallId) -> acp::ToolCallId {
    acp::ToolCallId::new(id.0.to_string())
}

fn reduced_text(output: &ReducedOutput) -> String {
    output.text.clone()
}

/// ACP's four permission options, in our order of preference.
///
/// docs/13's gate answers `Allow` / `AllowRemember` / `Deny`; ACP has a fourth
/// (`RejectAlways`). It is offered because an editor user who means "never ask me
/// about this again" has no way to say so otherwise, and it maps onto a *policy*
/// rule rather than a session answer — which is why `decision_of` reports it
/// distinctly rather than folding it into `Deny`.
pub fn permission_options() -> Vec<acp::PermissionOption> {
    vec![
        acp::PermissionOption::new(
            "allow_once",
            "Allow once",
            acp::PermissionOptionKind::AllowOnce,
        ),
        acp::PermissionOption::new(
            "allow_always",
            "Allow and remember",
            acp::PermissionOptionKind::AllowAlways,
        ),
        acp::PermissionOption::new("reject_once", "Deny", acp::PermissionOptionKind::RejectOnce),
        acp::PermissionOption::new(
            "reject_always",
            "Deny and remember",
            acp::PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// `PermissionRequest` → ACP's permission flow.
///
/// A separate function, not a `SessionUpdate` arm: this is a request the agent
/// blocks on, and treating it as a notification would let the loop run a tool
/// nobody approved.
pub fn permission_request(
    session: &acp::SessionId,
    event: &Event,
) -> Option<acp::RequestPermissionRequest> {
    let Event::PermissionRequest {
        call_id,
        tool,
        action,
        ..
    } = event
    else {
        return None;
    };
    Some(acp::RequestPermissionRequest::new(
        session.clone(),
        acp::ToolCallUpdate::new(
            call_id_of(call_id),
            acp::ToolCallUpdateFields::new()
                .title(format!("{tool}: {action}"))
                .kind(tool_kind(tool))
                // `Pending` is precisely "awaiting approval" in ACP.
                .status(acp::ToolCallStatus::Pending),
        ),
        permission_options(),
    ))
}

/// What the editor's answer means to the gate.
///
/// `RejectAlways` has no `PermDecision` — a remembered *denial* is a policy
/// change, not a turn answer — so it comes back flagged, and the caller (M16.5)
/// is forced to decide rather than silently downgrading it to a one-off deny.
pub fn decision_of(option_id: &str) -> Option<(PermDecision, bool)> {
    match option_id {
        "allow_once" => Some((PermDecision::Allow, false)),
        "allow_always" => Some((PermDecision::AllowRemember, false)),
        "reject_once" => Some((PermDecision::Deny, false)),
        "reject_always" => Some((PermDecision::Deny, true)),
        _ => None,
    }
}

/// Who ACP says decided: always the human at the editor.
pub fn decided_by() -> Actor {
    Actor::User
}

/// Every ACP update a *live* stream should send for these events.
///
/// Deltas are forwarded and the folded `AssistantMessage` is dropped: sending
/// both would print the assistant's answer twice in the editor.
pub fn live_updates(events: &[Envelope]) -> Vec<acp::SessionUpdate> {
    events
        .iter()
        .filter(|e| !matches!(e.event, Event::AssistantMessage { .. }))
        .filter_map(|e| session_update(&e.event))
        .collect()
}

/// Every ACP update a *replay* should send.
///
/// The mirror image: a persisted log has no deltas (they are ephemeral), so the
/// folded messages are the assistant's voice.
pub fn replay_updates(events: &[Envelope]) -> Vec<acp::SessionUpdate> {
    events
        .iter()
        .filter(|e| !matches!(e.event, Event::AssistantDelta { .. }))
        .filter_map(|e| session_update(&e.event))
        .collect()
}
