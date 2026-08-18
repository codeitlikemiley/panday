# 11 — ferrum-gateway

Every model call in the company goes through this service. It is the cost
meter, the policy gate, the cache, the failover switch, and the audit source —
*because* it is the only door (ADR-006). TensorZero's death taught the
positioning: this is not a product, it is the engine room of one.

## Responsibilities (in path order)

```
ChatRequest
  → authn (API key / service token)          — who
  → entitlements + budget check (ledger)     — may they    [fail CLOSED for API keys,
  → redaction hooks (optional per-tenant)    — DLP           fail OPEN for our harness]
  → route (ferrum-router)                    — where to
  → cache lookup (exact; semantic later)     — maybe free
  → provider adapter (stream)                — do it
  → usage capture (incl. cache splits)       — what it cost
  → ledger write + audit event               — remember it
  ← stream to caller
```

## Provider adapters

A small, sealed set — this is deliberately NOT a plugin surface (supply-chain
risk + dialect drift belongs to us):

| Adapter | Dialect | Notes |
|---|---|---|
| `anthropic` | Messages API | cache breakpoints, 1h TTL option, tool use |
| `openai` | Chat Completions + Responses | auto prefix caching ≥1024 tokens |
| `openai_compat` | Chat Completions | Together/Fireworks/Groq/vLLM/llama-server/mistral.rs — one adapter, many bases |
| `local` | openai_compat pinned to loopback | the offline tier; no auth |

Adapter contract: `fn chat(req: ChatRequest) -> impl Stream<StreamItem>` plus
`capabilities() -> Caps` (max context, tool support, cache style, modalities).
Conformance fixtures per dialect: recorded request/response pairs replayed in
CI; a provider API drift breaks a fixture, not production.

## Failover & hedging

- Route returns a **chain**, not a single target. On 5xx/timeout/overload:
  next target, with the *same* request (IR makes this possible).
- Mid-stream failure after tokens have flowed: emit `Error{retryable:true}`
  and let the harness decide (it holds turn semantics; the gateway does not
  re-prompt on its own).
- Hedged requests (fire second provider at p95 latency) are OFF by default —
  they double cache-write costs; enable per-route explicitly.
- Circuit breaker per (provider, model): trip on error-rate, half-open probes.

## Caching

- **Exact cache**: hash(normalized request) → response, PG unlogged table,
  TTL per route, only for `temperature=0` + no-tools requests (evals love
  this; agents rarely hit it). Redis only if PG p99 ever hurts.
- **Semantic cache**: pgvector over embedded prompts. Ships OFF; it is a
  correctness hazard for agentic traffic and mostly a demo feature. Revisit
  for the API product where customers opt in per key.
- **Provider prompt cache** is the real one: the gateway *preserves* client
  cache hints, adds Anthropic breakpoints per ADR-008, and reports cache
  reads and writes (split by TTL tier) in Usage so the ledger prices them at
  their real rates — reads ~0.1x, Anthropic writes 1.25x (5m) / 2x (1h),
  no write premium on automatic-caching providers.

## Quotas, budgets, stops

Entitlements (17) define: requests/min, tokens/day, spend ceiling, model
tier allowlist, max context. Enforced here pre-flight (estimate) and
post-flight (reconcile actual). Budget stop mid-session emits a typed
`budget_exceeded` the harness turns into a graceful session pause, not a 500.

## Also serves: OpenAI-compatible ingress

`POST /v1/chat/completions` accepting the standard dialect, mapped to IR.
Any existing tool (aider, continue.dev, curl scripts) can point at ferrum
with an API key and inherit routing/metering/caching. This is the platform's
cheapest adoption wedge and its best A/B harness (compare us vs direct).

## Non-goals

No business logic (harness owns turns), no transcript storage (log owns it),
no UI. The gateway is stateless apart from cache + circuit state → scales
horizontally behind any LB.

## Milestones

- **M11.1** IR + one adapter (openai_compat → llama-server): stream a local completion. ✅ *(shipped: `ferrum_sdk::providers::openai_compat` + the `OpenAiCompat` gateway adapter — sans-IO `SseDecoder` + `ChunkTranslator`, dialect mapping, and a `HttpStreamTransport` seam with a reqwest implementation. Tests mock the transport, so the suite passes with no model running. The wire layer moved out of `ferrum-gateway` in M10.2 so the SDK could share it.)*

- **M11.2** Anthropic adapter with cache breakpoints + usage splits; conformance fixtures for both. ✅ *(shipped: `ferrum_sdk::providers::anthropic` + the `Anthropic` gateway adapter; fixtures in `crates/ferrum-sdk/tests/fixtures/{openai_compat,anthropic}/`, replayed byte-at-a-time by `tests/conformance.rs`.)*

  **The M11.1 tool-call-id gap is closed.** `StreamItem::ToolCallStart` gained
  `provider_id` and `Message` gained `provider_call_id`; `Event::ToolCall`
  carries `provider_call_id` too, because state is a fold over the log and a
  session resumed from events alone must be able to answer a provider's tool
  call. All three are additive optional fields, so no `v` bump (docs/03
  §Versioning). Adapters now quote the provider's own id — `call_abc123` on
  Chat Completions, `toolu_01…` on Anthropic — instead of our UUID, which no
  provider ever issued.

  **Usage normalization is the load-bearing difference between the two.**
  Chat Completions reports `cached_tokens` as a subset of `prompt_tokens`;
  Anthropic reports fresh input, cache reads and cache writes as *disjoint*
  counts. The Anthropic adapter sums them into `input_tokens` per the
  `Usage` CONVENTION. Getting this wrong under-reports input by exactly the
  cached portion, which on a long agent session is most of it. Cache writes
  are split by TTL tier because they price differently (1.25x at 5m, 2x at
  1h); the flat `cache_creation_input_tokens` form is attributed to the 5m
  tier, since over-charging a tenant on a guess is worse than under-charging.
- **M11.3** Router integration (12) with chain-failover; kill-a-provider chaos test passes (session degrades, never errors to user).
- **M11.4** Ledger write path + budget stops; property test: Σ ledger == Σ provider-reported usage on replayed fixtures.
- **M11.5** OpenAI-compat ingress; aider-against-ferrum smoke test.
- **M11.6** Exact cache + circuit breakers; p99 overhead budget: <3ms non-streaming, <1ms per stream frame at 100 rps on one core.
