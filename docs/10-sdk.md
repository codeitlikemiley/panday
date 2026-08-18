# 10 — ferrum-sdk

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

## Layer 1 — Model IR (`ferrum-types::model`)

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
let client = Ferrum::builder()
    .base_url(env)                 // gateway URL; local daemon in offline mode
    .api_key(key)
    .middleware(Retry::default())  // tower stack: retry w/ jitter, timeout,
    .middleware(Metering::new())   //   usage capture, tracing propagation
    .build()?;

let mut stream = client.chat(req).await?;       // impl Stream<Item = StreamItem>
let vecs = client.embed(EmbedRequest { .. }).await?;
```

Everything is a `tower::Service` under the hood; middleware composes, and the
same stack runs inside the gateway's adapters (write once, use both sides).

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
#[ferrum::tool]                       // proc-macro: schema from types via schemars
/// Look up an order by id.
async fn lookup_order(ctx: &ToolCtx, order_id: String) -> Result<Order> { ... }

let agent = Agent::builder()
    .model("auto")                    // router decides
    .tools(tools![lookup_order])
    .policy(PermissionPolicy::allow_all())   // their process, their rules
    .build(client);
let run = agent.run(session, "where is order 123?").await?;
```

This embeds `ferrum-harness` (13) with in-memory event storage — the same
state machine that powers the cloud, which is the honesty guarantee: our
hosted product and the embedded SDK cannot drift because they are one crate.

## Errors

One `enum FerrumError` with stable `code` strings mirroring the wire:
`rate_limited { retry_after }`, `budget_exceeded { balance }`,
`entitlement_denied { plan, needed }`, `model_unavailable { tried: Vec<_> }`,
`permission_denied`, `provider { upstream, retryable }`. Retryability is a
method, not a guess.

## Milestones

- **M10.1** Model IR + streaming trait compile ✅ *(in workspace)*; round-trip serde tests.
- **M10.2** Gateway transport with retry/timeout middleware; streams a real completion end to end via one provider.
- **M10.3** Sessions client over WS with resume-after-seq; used by ferrum-cli (dogfood — the CLI has no private APIs).
- **M10.4** `#[ferrum::tool]` macro with schemars-derived schemas; compile-fail UI tests for bad signatures.
- **M10.5** Embedded Agent runs a 3-tool loop offline against `ferrum local`.
- **M10.6** Generated TS SDK from OpenAPI + AEP schemas; publish pipeline.

Acceptance across all: no public API returns a provider-specific type; a
change of provider behind the gateway is invisible in SDK-land.
