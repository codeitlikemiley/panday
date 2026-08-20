# 11 — panday-gateway

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
  → route (panday-router)                    — where to
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
| `anthropic` | Messages API | cache breakpoints, 1h TTL option, tool use. `ANTHROPIC_API_KEY`, or Claude Code subscription OAuth (`Authorization: Bearer` + `anthropic-beta: claude-code-20250219,oauth-2025-04-20`) |
| `xai` | openai_compat → `https://api.x.ai` | Grok CLI subscription OAuth (`~/.grok/auth.json`). Model id `xai/grok-4.6`. Not a proxy. |
| `openai` | Chat Completions + Responses | auto prefix caching ≥1024 tokens |
| `openai_compat` | Chat Completions | Together/Fireworks/Groq/vLLM/llama-server/mistral.rs — one adapter, many bases. Optional upstream via `PANDAY_BASE_URL` (alias `PANDAY_COMPAT_BASE_URL`) registers as `together/` |
| `local` | openai_compat pinned to loopback | the offline tier; no auth |

Subscription tokens are imported read-only by `panday_sdk::oauth` from the official CLIs' stores
and refreshed in memory. Never write `~/.grok/auth.json` or Claude Code credentials. A client
talking *to* panday ingress must send the full `provider/model` id (`OpenAiCompatClient::for_gateway`);
stripping `xai/` makes the router honestly refuse `grok-4.6`.

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
Any existing tool (aider, continue.dev, curl scripts) can point at panday
with an API key and inherit routing/metering/caching. This is the platform's
cheapest adoption wedge and its best A/B harness (compare us vs direct).

## Non-goals

No business logic (harness owns turns), no transcript storage (log owns it),
no UI. The gateway is stateless apart from cache + circuit state → scales
horizontally behind any LB.

## Milestones

- **M0.1** (defined in `docs/23-roadmap.md`) wired `Gateway` — the adapter
  registry, router integration and `UsageSink` that make the adapters below
  reachable as one call. It is the Phase 0 exit; the milestones here refine
  what it wires.

- **M11.1** IR + one adapter (openai_compat → llama-server): stream a local completion. ✅ *(shipped: `panday_sdk::providers::openai_compat` + the `OpenAiCompat` gateway adapter — sans-IO `SseDecoder` + `ChunkTranslator`, dialect mapping, and a `HttpStreamTransport` seam with a reqwest implementation. Tests mock the transport, so the suite passes with no model running. The wire layer moved out of `panday-gateway` in M10.2 so the SDK could share it.)*

- **M11.2** Anthropic adapter with cache breakpoints + usage splits; conformance fixtures for both. ✅ *(shipped: `panday_sdk::providers::anthropic` + the `Anthropic` gateway adapter; fixtures in `crates/panday-sdk/tests/fixtures/{openai_compat,anthropic}/`, replayed byte-at-a-time by `tests/conformance.rs`.)*

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
- **M11.3** Router integration (12) with chain-failover; kill-a-provider chaos test passes (session degrades, never errors to user). ✅ *(shipped: `Gateway::resolve_chain` + failover in `Gateway::chat`; `crates/panday-gateway/tests/failover_chaos.rs`.)*

  The chaos test runs a six-turn session, kills the head provider a third of
  the way through and revives it near the end, and asserts every turn was
  served with content and every one was billed. "Never errors to user" is the
  property, which is stronger than "eventually succeeds".

  **Failover covers establishment only.** Once a stream exists a mid-stream
  failure is surfaced, never retried — the harness holds turn semantics, and
  re-prompting would double-bill the caller for tokens they already saw. A
  test asserts the next leg is not touched after partial content has flowed.

  **A non-retryable failure does not walk the chain.** A 400 fails identically
  on every target, so trying the rest would turn one clear error into several
  confusing ones while spending the caller's quota to do it.

  An exhausted chain reports every leg it tried with the upstream reason, so an
  operator can tell one bad provider from a global outage.
- **M11.4** Ledger write path + budget stops; property test: Σ ledger == Σ provider-reported usage on replayed fixtures. ✅ *(shipped: `panday_gateway::BudgetGate`, `panday_platform::ledger::{LedgerSink, LedgerBudget}`; suite in `crates/panday-platform/tests/ledger_write_path.rs`, integration lane.)*

  **`UsageSink` became async**, and that is the interesting part. docs/17 says usage is "written in
  the request path by gateway (usage.model)", and a synchronous callback can only buffer — which
  would make every deployment fail-open whether it meant to or not. docs/17 wants that to be a
  per-surface *choice* ("fail-closed for API keys, fail-open-with-alarm for our own interactive
  surfaces"), so it is a field on the sink, and the fail-open path logs at `error!` because an
  outage that produces no alarm is an outage nobody backfills.

  **The budget gate runs before the cache.** An account over its ceiling must be refused rather
  than served for free, or "you are over your limit" and "here is a cached answer" become the same
  request depending on who asked first.

  **Two claims the ledger keeps apart.** An *unpriced* model produces no entry at all (docs/21's
  rule applied to money: "free" and "unpriced" are different, and a zero-cost entry would
  understate COGS with nothing downstream able to tell), while a model priced *at* zero — a local
  one — produces an entry of zero, because the call still happened and docs/18 meters it. Margin is
  applied once, in the sink, with the provider's cost kept beside it in `quantity`: COGS and the
  price charged are different numbers and a ledger storing one cannot answer either question.

  **The property test is the acceptance**: Σ ledger == Σ priced provider usage, to the
  micro-credit, over seven call shapes chosen for where the errors live — cache-heavy,
  output-heavy, free, and unpriced. It also reconciles the other way, summing
  `provider_cost_micros` out of the entries, which is what a dispute would actually be settled
  with.

  **An ordering bug the tests found.** At 90% of the ceiling the soft rule wants to demote to a
  cheaper pool; a request whose own estimate crosses the ceiling was getting that answer instead of
  a refusal. Demoting changes *which pool serves the call*, not what the account may spend, so the
  hard check now runs against balance-plus-estimate before the soft one — otherwise "degrade" is a
  way to afford something the ceiling had ruled out.
- **M11.5** OpenAI-compat ingress; aider-against-panday smoke test. ✅ *(shipped: `panday_gateway::ingress` — `POST /v1/chat/completions`, streaming and buffered; served by the `panday-gateway` binary.)*

  The A/B property is why the surface must be *exactly* the standard dialect: a
  client has to be redirectable by changing a base URL and nothing else, or the
  comparison is not like-for-like. So the tests speak **raw HTTP**, not a typed
  client — that is what aider and a curl script actually do, and it is the only
  way to catch a response our own types would round-trip happily and a real
  parser would reject. Every SSE frame is asserted to be valid JSON, `stop`
  accepts both a string and an array, and an assistant message with no
  `content` is accepted (any transcript that used tools has one).

  Verified live against a local server with `curl`, not only in tests.

  **Two things this milestone exposed.** `auto` never reached a task rule,
  because the ingress leaves `metadata.task` unset and the gateway passed that
  straight through — so M12.3's classifier existed but nothing called it, and
  every external request fell to the default pool. The gateway now classifies
  when the caller did not declare, which is precisely how an external client
  inherits routing without knowing it exists.

  And an exhausted chain of rate limits used to surface as `ModelUnavailable`
  (503), stripping the one signal a client can act on. If **every** leg was
  rate-limited the gateway now returns `RateLimited` (429) so back-off still
  works; a mixed set of failures stays 503.

  *Not done:* the literal aider smoke test needs aider installed. `curl` and the
  raw-HTTP suite exercise the same surface.
- **M11.6** Exact cache + circuit breakers; p99 overhead budget: <3ms non-streaming, <1ms per stream frame at 100 rps on one core. ✅ *(shipped: `panday_gateway::cache`, `panday_gateway::circuit`, wired in `Gateway::chat`; `tests/cache_and_breakers.rs`, `tests/overhead.rs`.)*

  **Measured, release build, single-threaded runtime, 1000 back-to-back requests
  (harder than the specified 100 rps — no idle time between them):**
  establishment p50 21.6µs / p99 52.0µs against the 3ms budget; per stream frame
  p50 41ns / p99 1.17µs against the 1ms budget. The benchmark is `#[ignore]`d
  because a wall-clock assertion on shared CI hardware fails for reasons
  unrelated to the code, and a flaky test gets muted — which is worse than one
  that must be run deliberately.

  **The exact cache is not Postgres yet.** The spec names a PG unlogged table and
  PG is M3.5, so what shipped is the `ExactCache` trait plus an in-memory
  implementation (bounded, TTL, expire-on-read), and the binaries wire whichever
  they have. The parts with the bugs in them — eligibility, key normalization,
  tenant scoping — are the same either way.

  **The cache key is tenant-scoped by construction**, which is the finding
  docs/20's M20.3 cache-key audit exists to make: a key without the account would
  let one tenant's prompt serve another tenant's response, and identical prompts
  across tenants are exactly what a shared eval harness produces. The leak would
  have looked like a cache working well.

  Two eligibility rules from docs/11 with teeth: **absent temperature is not
  zero** (providers default to ~1, so treating unset as deterministic would cache
  one sample of a distribution and serve it forever — a model that appears to stop
  thinking), and **a request with tools is never cached**, because replaying a
  cached tool call would have the harness act on a decision made about a different
  workspace. Caching is off unless a TTL is configured; a gateway told nothing
  about TTLs has not been asked to serve stale answers.

  A hit still emits a `Usage` frame, rewritten so the tokens read as cache reads
  with zero output. Replaying the original usage would bill twice for one
  purchase; dropping the frame would make the request vanish from a client's own
  accounting. And a response is stored on `Done`, never at stream end: an
  abandoned stream is a partial answer, and caching it would serve a truncated
  response to everyone who asked afterwards.

  **Breakers are per (provider, model) and trip on error *rate*, not count.** A
  count trips on volume, so a busy route at 1% errors would open before a quiet
  route failing everything. The rate needs a minimum sample size, or the first
  failure on a cold route is a 100% error rate. Three further judgements: a
  half-open probe is *reserved* by the same call that checks it, so a burst sends
  one probe rather than all of them; a probe's own result decides the breaker
  rather than the rate, because the window still holds the failures that opened
  it; and a non-retryable error (a malformed request) is not counted against the
  route, or one broken client could take a healthy model offline for everyone.
