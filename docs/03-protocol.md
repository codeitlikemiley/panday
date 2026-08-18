# 03 — Event Protocol & Wire Contracts

The platform has exactly three wire surfaces. Everything else is internal.

1. **AEP** (Agent Event Protocol) — the session event stream. North-facing:
   clients render it; the harness emits it; the log stores it.
2. **Platform REST** — control plane: auth, sessions CRUD, billing, keys.
3. **Model-plane HTTP** — south-facing: the gateway's provider adapters speak
   each provider's dialect; the gateway itself also *serves* an
   OpenAI-compatible endpoint so any existing tool can point at it.

Plus two adopted protocols we conform to rather than define: **MCP** (rmcp)
for tools, **ACP** (agent-client-protocol) for editors. The CLI/harness maps
AEP⇄ACP; the mapping is mechanical because both are event-shaped.

## AEP: the event log IS the session

Rust definitions live in `ferrum-types::event` (the workspace compiles them
today); JSON Schema is exported to `proto/` by `cargo xtask schemas`. The
essentials:

```rust
pub struct Envelope {
    pub v: u16,               // protocol version, currently 1
    pub session_id: SessionId,
    pub seq: u64,             // gapless, per-session; THE ordering primitive
    pub turn_id: Option<TurnId>,
    pub at: Timestamp,
    pub event: Event,         // the payload, tagged enum
}

pub enum Event {
    // conversation
    UserMessage { content: Vec<ContentBlock>, source: ClientKind },
    AssistantDelta { text: String },            // streaming only, not persisted
    AssistantMessage { content: Vec<ContentBlock>, usage: Usage },
    // tools  (tool names are plain strings in v1; a ToolName newtype is a
    // planned tightening, not a shipped one)
    ToolCall { call_id: CallId, tool: String, args: Json,
               provider_call_id: Option<String> },  // the provider's opaque id
    ToolResult { call_id: CallId, output: ReducedOutput, raw_ref: Option<ArtifactRef>,
                 duration_ms: u64, is_error: bool },
    // control
    PermissionRequest { call_id: CallId, tool: String, action: String, options: Vec<String> },
    PermissionDecision { call_id: CallId, decision: PermDecision, by: Actor },
    // context economy
    Compaction { from_seq: u64, to_seq: u64, summary_ref: ArtifactRef,
                 tokens_before: u32, tokens_after: u32 },
    // lifecycle
    TurnStarted { model: ModelRef, parent: Option<TurnId> },
    TurnFinished { reason: StopReason, usage: Usage, cost_micros: u64 },
    SubagentSpawned { child: SessionId, brief: String },
    SubagentFinished { child: SessionId, result_ref: ArtifactRef },
    SessionForked { from_seq: u64 },
    Error { code: String, message: String, retryable: bool },
    // read-tolerance: an event kind this build does not know. Captured
    // verbatim (tag included) so it survives a read/write cycle unchanged.
    // Never constructed deliberately — see §Versioning discipline.
    Unknown { payload: serde_json::Map<String, Json> },
}
```

Design rules, each one load-bearing:

- **Two ids per tool call.** `call_id` is ours (UUIDv7, the log's key);
  `provider_call_id` is the provider's opaque token (`call_abc123`,
  `toolu_01…`), which must be quoted verbatim to answer the call. It lives on
  the event, not just in adapter memory, because a session resumed from the
  log alone must still be able to reply (M11.2).
- **Append-only, gapless `seq`.** Clients resume with `?after_seq=N`; the
  server replays. There is no other sync mechanism and none is needed.
- **Deltas are ephemeral; messages are durable.** `AssistantDelta` streams to
  connected clients but only the folded `AssistantMessage` is persisted. Keeps
  the log compact and replay deterministic.
- **Big payloads live in object storage.** Events carry `ArtifactRef` (a
  content-addressed handle), never megabytes. The reducer decides what is
  inline vs spilled (`15-reducer.md`).
- **Every event that costs money carries `Usage`** so the ledger can be rebuilt
  from the log alone — billing disputes are settled by replay, not by trust
  in a counter. Counting rule: per-model-call usage lives on
  `AssistantMessage`; `TurnFinished.usage` is a redundant turn summary (must
  equal the sum, property-tested at M3.5) — folds count the former only.
- **State = fold(log).** The harness holds a materialized `SessionState` in
  memory, but any component (and any test) may rebuild it from events. If a
  state can't be reconstructed from the log, the missing event is a bug.

### Transport

WebSocket (bidirectional: client also *sends* `UserMessage` /
`PermissionDecision`) with SSE fallback for read-only surfaces. JSON v1;
envelope versioning gives us a path to CBOR/protobuf if profiling ever
demands it (don't pre-optimize this).

### Versioning discipline

- Additive fields: always ok (serde `#[serde(default)]` everywhere on read).
- New event kinds: minor; unknown kinds MUST be ignored-and-preserved by
  clients (test this — send a fake kind in the golden suite).
- Field removal/retype: bump `v`, dual-write for one release. Should be rare
  to never.

**How ignore-and-preserve is implemented** (M3.1): `Event::Unknown` is a
`#[serde(untagged)]` fallback variant holding the raw object. An unrecognised
`event` tag deserializes into it instead of failing, and re-serializing emits
the original bytes — so an older client can relay or re-persist a newer
server's log without losing events it cannot interpret. `Event::kind()` reports
the tag either way; `Event::is_unknown()` distinguishes them. Known tags are
listed in `KNOWN_EVENT_TAGS`, and the golden suite asserts every known variant
has a fixture and that no known kind falls through to `Unknown`.

## Platform REST (sketch — full OpenAPI generated in M-plat milestones)

```
POST   /v1/sessions                     create (returns session + ws url)
GET    /v1/sessions/:id/events?after_seq=  replay/tail
POST   /v1/sessions/:id/messages        send user message (non-ws clients)
POST   /v1/sessions/:id/fork            branch from seq
GET    /v1/models                       what the router will admit for this key
POST   /v1/chat/completions             OpenAI-compatible passthrough (gateway)
GET    /v1/usage                        ledger view for the caller
POST   /v1/keys · GET /v1/keys · DELETE /v1/keys/:id
```

Auth: `Authorization: Bearer` — API keys (`frm_live_…`, hashed at rest) for
programmatic; OIDC-backed short-lived JWTs for interactive clients. Same
entitlement checks either way (`17-platform.md`).

## Identifiers

UUIDv7 for `session_id`/`turn_id`/`call_id` (time-ordered → PG-index-friendly,
trivially globally unique). Human-facing short forms (`sess_01J…`) are a
display encoding, not a second id.

## Milestones

- **M3.1** `ferrum-types` events compile + serde round-trip; golden fixtures checked in. ✅ *(shipped: 17 fixtures in `crates/ferrum-types/tests/fixtures/`, harness in `tests/golden.rs`, plus the versioning-discipline suite — additive fields, unknown-kind tolerance, version pin.)*
- **M3.2** `cargo xtask schemas` exports JSON Schema to `proto/`; CI diffs it (a schema change without a version note fails). ✅ *(shipped: `xtask/`, `proto/aep-envelope.schema.json`, `cargo xtask schemas --check` in CI.)*
- **M3.3** WS endpoint in harnessd: create session, stream events, resume-after-seq proven by killing the connection mid-turn.
- **M3.4** Unknown-event tolerance test in CLI; ACP mapping table implemented for the core six events.
- **M3.5** Ledger-rebuild-from-log: property test that replaying any session yields the ledger totals the live path recorded.
