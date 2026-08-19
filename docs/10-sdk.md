# 10 — panday-sdk

The client library. Three audiences, one crate: (a) our own services calling
models through the gateway, (b) our clients (CLI, future web) talking to the
platform, (c) paying customers embedding agents in their Rust apps. TS and
Python SDKs are *generated* from the REST/AEP contracts later — Rust is the
hand-written reference.

## Prior art, and why we still write our own

`rig` (rig-core 0.42, the most mature Rust agent library, now with a sans-IO
AgentRun state machine), `genai`, `async-openai` are all healthy in 2026. We
crib patterns from rig's sans-IO design, and `async-openai` remains a fine
dependency for one adapter. But the SDK's *job* here is to be the typed face
of OUR platform — model IR, AEP sessions, entitlement-aware errors — which no
general-purpose crate models. The provider-adapter layer we'd reuse is the
easy 20%.

## Layer 1 — Model IR (`panday-types::model`)

One request/response vocabulary across Anthropic, OpenAI-compat, and local:

```rust
pub struct ChatRequest {
    pub model: ModelRef,              // "anthropic/claude-...", "local/qwen3.5-4b", "auto"
    pub messages: Vec<Message>,       // system separated; roles: user/assistant/tool
    pub tools: Vec<ToolDef>,          // name, description, JSON Schema params
    pub sampling: Sampling,           // temp, top_p, max_tokens, stop
    pub cache: CacheHints,            // breakpoint positions for Anthropic-style caching
    pub stream: bool,
    pub metadata: CallMeta,           // tenant, session, turn, correlation — REQUIRED
}

pub enum StreamItem {
    Delta(String),
    ToolCallStart { id: CallId, name: ToolName },
    ToolCallDelta { id: CallId, args_fragment: String },
    Usage(Usage),                     // final; includes cache_read/cache_write splits
    Done(StopReason),
}
```

Rules: `CallMeta` is non-optional — an unattributed model call is a compile
error, which is how cost attribution stays total. `Usage` carries
`input`, `output`, `cache_read`, `cache_write` as separate counts because the
ledger prices them differently (ADR-007/008).

## Layer 2 — Transport client

```rust
let client = Panday::builder()
    .base_url(env)                 // gateway URL; local daemon in offline mode
    .api_key(key)
    .middleware(Retry::default())  // tower stack: retry w/ jitter, timeout,
    .middleware(Metering::new())   //   usage capture, tracing propagation
    .build()?;

let mut stream = client.chat(req).await?;       // impl Stream<Item = StreamItem>
let vecs = client.embed(EmbedRequest { .. }).await?;
```

Middleware composes, and the same stack runs inside the gateway's adapters
(write once, use both sides) — the wire layer lives in
`panday_sdk::providers`, which `panday-gateway`'s adapters wrap.

**Implemented as `ModelClient` decorators, not `tower::Service`** (M10.2).
Retry can only ever wrap the call that *establishes* a stream, never the
stream itself: docs/11 requires that a mid-stream failure emit
`Error{retryable:true}` and let the harness decide, because it holds turn
semantics. That puts the retry boundary exactly at `ModelClient::chat` — one
`async fn` returning `Result<ItemStream, _>` — where `poll_ready`/`call` buys
nothing and tower's `Retry` would need a response it can inspect, which a
boxed stream is not. Tower stays on the HTTP ingress side (axum + tower,
docs/02), which is where M11.5 uses it.

Layering order is `.with_timeout(..).with_retry(..)`: retry outside the
timeout, so every attempt gets its own deadline. The timeout bounds
time-to-first-stream, not the stream's lifetime — a model generating for two
minutes is working, a gateway silent for ten seconds is not.

## Layer 3 — Sessions (the platform surface)

```rust
let session = client.sessions().create(SessionOpts::coding()).await?;
let mut events = session.subscribe(After::Latest).await?;   // AEP stream
session.send(UserInput::text("fix the failing test")).await?;
while let Some(ev) = events.next().await {
    match ev.event {
        Event::PermissionRequest { .. } => session.decide(..).await?,
        Event::AssistantMessage { .. } => { ... }
        _ => {}
    }
}
```

Resume is `subscribe(After::Seq(n))` — no separate sync API, per ADR-002.

## Layer 4 — Embedded agent (harness-lite)

For customers who want the loop in-process rather than calling our hosted
harness:

```rust
/// Look up an order by id.
#[panday_sdk::tool]                   // proc-macro: schema from types via schemars
async fn lookup_order(ctx: &ToolCtx, order_id: String, verbose: Option<bool>)
    -> Result<Order, String> { ... }

let mut registry = ToolRegistry::default();
registry.register(Box::new(LookupOrder));   // the type the macro generated

let agent = Agent::builder()
    .model("auto")                    // router decides
    .tools(registry)
    .policy(PermissionPolicy::allow_all())   // their process, their rules
    .build(client);
let run = agent.run(session, "where is order 123?").await?;
```

**Amended at M10.4: the macro generates `LookupOrder`, it does not take over
`lookup_order`.** The original sketch's `tools![lookup_order]` implies a unit struct
named after the function, which would occupy the value namespace the function lives
in — so the function would no longer be callable, including from its own unit tests. A
tool you can only reach through an agent loop is a tool whose business logic can only
be tested through an agent loop. The function stays exactly as written; the macro adds
`LookupOrder` (the `Tool`) and `LookupOrderArgs` (the schema).

This embeds `panday-harness` (13) with in-memory event storage — the same
state machine that powers the cloud, which is the honesty guarantee: our
hosted product and the embedded SDK cannot drift because they are one crate.

## Errors

One `enum PandayError` with stable `code` strings mirroring the wire:
`rate_limited { retry_after }`, `budget_exceeded { balance }`,
`entitlement_denied { plan, needed }`, `model_unavailable { tried: Vec<_> }`,
`permission_denied`, `provider { upstream, retryable }`. Retryability is a
method, not a guess.

## Milestones

- **M10.1** Model IR + streaming trait compile ✅ *(in workspace)*; round-trip serde tests.
- **M10.2** Gateway transport with retry/timeout middleware; streams a real completion end to end via one provider. ✅ *(shipped: `panday_sdk::gateway::GatewayTransport` + `connect()`, `panday_sdk::middleware::{Retry, Timeout}`. The wire layer moved from `panday-gateway` to `panday_sdk::providers` so both sides share it.)*
- **M10.3** Sessions client over WS with resume-after-seq; used by panday-cli (dogfood — the CLI has no private APIs). ✅ *(shipped: `panday_sdk::sessions`, `panday-harnessd`'s `SessionDriver` + inbound socket handling, `panday session` in the CLI; suites in `crates/panday-sdk/tests/sessions.rs` and `crates/panday-cli/tests/session_dogfood.rs`.)*

  **The socket is bidirectional**, and that is a design decision rather than a
  convenience. An earlier shape had events arriving on the WS and input going over
  POST, which gives two orderings to reason about — an input accepted after a
  disconnect but before its events — and leaves the client unable to tell whether
  its input landed before the events it is missing. One socket means one order: what
  a client sends is sequenced against what it receives, and if the socket dies both
  halves die together and resume replays the truth.

  **Resume is the only sync mechanism** (docs/03), so the client has no reconcile
  step, no "am I in sync" handshake and no local queue. `resume_point()` advances
  only when an event is *returned to the caller*, never when it is received — an
  event dropped between the socket and the application must be replayed, and
  advancing on receipt would skip it. The acceptance case is docs/03's: the test
  *drops* a live connection mid-turn rather than closing it politely, then resumes
  and asserts it receives exactly the missed events, no repeats and no gap.

  A resume point past the head is surfaced as a **protocol** error and reported
  non-retryable, so a client does not loop against a condition retrying cannot fix.
  Input to a server with no driver is refused with a close reason rather than
  dropped: a client whose message vanished would wait forever for events that were
  never coming. Same for a malformed frame.

  `panday-harnessd` takes a `SessionDriver` rather than embedding a `SessionActor`
  (docs/01: "libraries take traits, binaries do the wiring") — which model, tools and
  sandbox a hosted session gets is a deployment decision, and it is what lets this
  suite drive a real socket against a scripted session with no provider.

  **The dogfood is a test, not a claim.** `panday session` reaches the platform only
  through `panday_sdk::sessions`, and it renders events with the *same*
  `replay::Renderer` that `panday replay` uses — so a live session and its replay are
  the same text rather than two renderings of the same facts. Its second invocation
  resumes from the printed `--after-seq`, which is how that flag gets exercised the
  way a person would use it.
- **M10.4** `#[panday::tool]` macro with schemars-derived schemas; compile-fail UI tests for bad signatures. ✅ *(shipped: `crates/panday-macros`, re-exported as `panday_sdk::tool`; suite and eight UI fixtures in `crates/panday-harness/tests/tool_macro.rs` + `tests/ui/`.)*

  **The parameter list is the schema.** The macro builds an args struct from the
  parameters after `ctx` and derives `JsonSchema` from that, so a signature and its
  schema cannot disagree — which is the entire reason to have the macro rather than a
  hand-written `ToolSpec` next to each function. Required-ness comes from `Option`,
  the reading a Rust developer already has. The description comes from the `///` doc
  comment: it lands in the stable cached prefix (ADR-008) and is what the model reads
  to decide whether to call, so taking it from the doc comment means one description
  rather than two that drift.

  **`deny_unknown_fields` is deliberate.** A misspelled argument that was silently
  dropped would look like the tool ignoring its instructions, which is the hardest
  kind of bug to see in a transcript. It comes back as a tool error naming the field,
  which the next turn can fix.

  **The error messages are the deliverable**, so they are the thing under test. Eight
  fixtures cover not-async, generic, no-`Result`, missing `ctx`, `self`, a borrowed
  argument, a pattern argument and an unknown `side_effects` value — each one asserting
  that the error points at the developer's own tokens and says why the rule exists. A
  macro that accepts a bad signature and then fails inside its own expansion produces an
  error pointing at code nobody wrote, which is the experience these prevent.
  Regenerate with `TRYBUILD=overwrite cargo test -p panday-harness --test tool_macro`.
  The suite builds a scratch crate, so its first run in a cold CI cache costs a couple
  of minutes; afterwards it is ~1s.

  **Generated code routes through `panday_harness::__private`** (serde, serde_json,
  schemars, async_trait). Without that, every tool author would have to add four
  unrelated crates to their manifest and keep the versions in step with ours; with it,
  a crate defining tools depends on `panday-harness` and `panday-sdk` and nothing else.
- **M10.5** Embedded Agent runs a 3-tool loop offline against `panday local`. ✅ *(shipped: `panday_harness::Agent`; suite in `crates/panday-harness/tests/embedded_agent.rs`.)*

  **It lives in `panday-harness`, not `panday-sdk`.** This section documents it as the SDK's
  Layer 4 and also says it "embeds `panday-harness` ... the same state machine that powers
  the cloud, which is the honesty guarantee". Both cannot be literally true of one crate:
  `panday-harness` depends on `panday-sdk` for `ModelClient`, so the reverse is a cycle. It
  sits next to the state machine it wraps, and an embedder depends on both — the same
  arrangement `#[panday_sdk::tool]` already has.

  **It is a builder over `SessionActor` and nothing else.** No second loop, no second gate,
  no second reducer: anything an embedded agent did differently would be drift, which is the
  thing this design exists to prevent. It even keeps the shipping reducer stack, so the same
  input gives the same answer hosted or embedded.

  **The default policy is `Dev`, not `Unleashed`.** An embedded agent with no gate is a
  library that can email a customer because a model asked; the caller who wants that types
  it. And an `Irreversible` tool asks in *every* profile (docs/13 M13.5) — the three-tool
  loop test uses a mutating-but-replayable third tool for exactly that reason, which is a
  detail the first draft got wrong and the suite caught.

  **A paused run is not a finished run.** `Run::finished()` distinguishes them and the stop
  reason for a park is `ToolUse` rather than `EndTurn`: a caller that conflated the two would
  report a task complete that never ran.

  "Offline" is a loopback OpenAI-compatible server — the same thing `panday local` talks to
  (docs/18: the local adapter "doesn't care which"). CI's is a fake, because CI has no GGUF;
  what is real is the loop, the three tools, the gate, the reducer and the log. The tools in
  the suite are defined with `#[panday_sdk::tool]` so the seam between M10.4 and M10.5 is
  covered rather than assumed, and the run's events render with `panday replay`'s own
  renderer — which is the observable form of "the same state machine".
- **M10.6** Generated TS SDK from OpenAPI + AEP schemas; publish pipeline.

Acceptance across all: no public API returns a provider-specific type; a
change of provider behind the gateway is invisible in SDK-land.
