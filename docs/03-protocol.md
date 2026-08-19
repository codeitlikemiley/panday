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

Rust definitions live in `panday-types::event` (the workspace compiles them
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

Auth: `Authorization: Bearer` — API keys (`pnd_live_…`, hashed at rest) for
programmatic; OIDC-backed short-lived JWTs for interactive clients. Same
entitlement checks either way (`17-platform.md`).

## Identifiers

UUIDv7 for `session_id`/`turn_id`/`call_id` (time-ordered → PG-index-friendly,
trivially globally unique). Human-facing short forms (`sess_01J…`) are a
display encoding, not a second id.

## Milestones

- **M3.1** `panday-types` events compile + serde round-trip; golden fixtures checked in. ✅ *(shipped: 17 fixtures in `crates/panday-types/tests/fixtures/`, harness in `tests/golden.rs`, plus the versioning-discipline suite — additive fields, unknown-kind tolerance, version pin.)*
- **M3.2** `cargo xtask schemas` exports JSON Schema to `proto/`; CI diffs it (a schema change without a version note fails). ✅ *(shipped: `xtask/`, `proto/aep-envelope.schema.json`, `cargo xtask schemas --check` in CI.)*
- **M3.3** WS endpoint in harnessd: create session, stream events, resume-after-seq proven by killing the connection mid-turn. ✅ *(shipped: `panday-harnessd` — `POST /v1/sessions`, `GET /v1/sessions/{id}/ws?after_seq=`, `GET /v1/sessions/{id}/events?after_seq=`.)*

  The test kills a real TCP connection mid-turn, lets work continue while
  nobody is listening, reconnects with the last `seq` the client actually saw,
  and asserts it receives exactly the missed events in order.

  **Two ordering rules the implementation turns on:**

  - The live subscription is taken **before** the log is read, so an event
    appended between replay and tail is delivered rather than lost. A
    duplicate is filtered by `seq`; a gap could never be recovered.
  - Events are made durable **before** fan-out. A client must never see an
    event that is not in the log, or a later resume would appear to *lose* it.

  **`after_seq` beyond the log's head is refused with 409.** It used to be
  accepted, and then every future event was filtered as "already sent" — the
  client sat receiving nothing, forever, with no error. A resume point that
  does not exist means the client's state is impossible, and saying so beats
  hiding it.

  A lagging client is disconnected rather than skipped: it can reconnect with
  `after_seq` and recover everything from the log, whereas a skipped event is
  a gap it could never detect.

  Storage here is the in-memory `EventStore`. The Postgres-backed store needs
  a live database and belongs to **M2.3** (the integration lane with PG+MinIO
  compose), which is where `sqlx`'s compile-time-checked queries land.
- **M3.4** Unknown-event tolerance test in CLI; ACP mapping table implemented for the core six events. ✅ *(shipped: `panday_cli::acp`, tests in `crates/panday-cli/tests/acp_mapping.rs` and `panday replay`'s own suite.)*

  docs/16 calls the mapping "mechanical", which is true of the shape and not of
  three seams:

  - **A tool call is one ACP entity across two AEP events.** `ToolCall` and
    `ToolResult` are separate log entries; ACP models one `toolCallId` that starts
    `in_progress` and is *updated* to `completed`/`failed`. Emitting two
    `tool_call` updates would make an editor draw the same call twice, so both
    events derive the id through one function.
  - **A permission request is not a session update.** It is a request *to* the
    client that the agent blocks on, so it maps to `RequestPermissionRequest` and
    `session_update` returns `None` for it. As a notification it would let the
    loop run a tool nobody approved — the one bug here that costs more than a
    rendering glitch.
  - **ACP has a fourth permission answer we do not.** `reject_always` is a
    remembered *denial*, which is a policy change rather than a turn answer, so
    `decision_of` returns it flagged instead of folding it into `Deny`.

  Deltas and folded messages are the same text, so a live stream sends the deltas
  and a replay sends the folded messages; sending both prints the answer twice.
  Tool results carry the *reduced* text — an editor showing what the model never
  saw would be debugging a different session.

  Assertions are made against serialized ACP JSON, not the Rust types: an editor
  parses bytes, and a mapping that type-checks while emitting the wrong field name
  does not work. The types come from the official crate's `schema::v1` module
  rather than its root, which pins the protocol version explicitly — the crate
  also carries a v2 draft, and drifting onto it would change the wire format
  silently.

  This is the table only; stdio transport, the `session/new` handshake and the
  answer round trip are M16.5.
- **M3.5** Ledger-rebuild-from-log: property test that replaying any session yields the ledger totals the live path recorded. ✅ *(shipped: `panday_platform::rebuild`; suite in `crates/panday-platform/tests/rebuild_from_log.rs`, integration lane.)*

  This is what makes docs/17's claim that the ledger is **provable** true rather than aspirational:
  "a dispute is settled by replaying the session and recomputing". Two hundred generated sessions
  run through the *real* path — fake provider, real gateway, real `LedgerSink` writing to Postgres —
  and then the log alone is replayed and the totals compared. Discrepancy: zero, to the
  micro-credit.

  **A property test without a property-testing crate.** `proptest` is not in docs/02's table, and
  the shrinking it buys is worth less here than the shapes: what breaks a ledger rebuild is cache
  splits, a model change mid-session, unpriced models and zero-cost calls — enumerable rather than
  discoverable. So the generator is a small deterministic LCG over those shapes, seeded, so a
  failure is reproducible from its seed. The test also asserts the generator *produced* the
  interesting cases (>40 sessions changing model, >20 with an unpriced model), because 200 copies of
  the easy case would pass while proving nothing.

  **Two rules make the fold correct, and both are bugs if reversed.** Usage comes from
  `AssistantMessage` only — `TurnFinished.usage` is a redundant summary, so counting both doubles
  every turn, and the log the test builds includes it precisely so a rebuild that counted it would
  fail. And each turn is priced at the model *that turn* used, from `TurnStarted`: pricing a session
  at its first model disagrees with the live path on exactly the sessions that failed over, which
  are the ones a dispute is most likely to be about.

  **An unpriced turn is reported, not skipped.** A rebuild that silently ignored one would "agree"
  with a live path that had also ignored it, and neither would be right — so `Rebuilt` carries
  `unpriced_turns` and the token counts, making the gap visible. The discrepancy is signed for the
  same reason: over-billing and under-billing are different incidents with different responses.
