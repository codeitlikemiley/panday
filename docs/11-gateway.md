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
| `anthropic` | Messages API | `ANTHROPIC_API_KEY` and/or `PANDAY_ANTHROPIC_API_KEYS` (comma-separated), plus Claude Code OAuth. Rotated in one pool (docs/25). |
| `xai` | openai_compat → `https://api.x.ai` | Grok CLI OAuth (`~/.grok/auth.json`, `PANDAY_GROK_AUTH`) and/or `XAI_API_KEY` / `PANDAY_XAI_API_KEYS`. Console: `GET /accounts`. |
| `openai` | Chat Completions | `OPENAI_API_KEY` and/or `PANDAY_OPENAI_API_KEYS`. Models `gpt-5.6-sol` / `gpt-5.6-terra` / `gpt-5.6-luna` |
| `gemini` | openai_compat → Google OpenAI layer | `GEMINI_API_KEY` and/or `PANDAY_GEMINI_API_KEYS`. Optional `GEMINI_BASE_URL`. |
| `openai_compat` | Chat Completions | Together/Fireworks/Groq/vLLM/llama-server/mistral.rs — one adapter, many bases. Optional upstream via `PANDAY_BASE_URL` (alias `PANDAY_COMPAT_BASE_URL`) registers as `together/` |
| `local` | openai_compat pinned to loopback | the offline tier; no auth. Registered only when that URL answers (default `http://127.0.0.1:8081`, or `PANDAY_LOCAL_BASE_URL`) |

Subscription tokens are imported read-only by `panday_sdk::oauth` from the official CLIs' stores
and refreshed in memory. Never write `~/.grok/auth.json` or Claude Code credentials. A pool of
several keys or subscriptions is `docs/25-credentials.md` — put them in with `panday creds`
(stdin or `--from-grok` / `--from-claude` / `--from-codex`). `GET /accounts` on the operator
console imports Grok CLI logins, accepts a pasted `auth.json`, and accepts extra OpenAI /
xAI / Anthropic / Gemini API keys. `PANDAY_ROTATE=failover|round_robin` (same control on
that page). Extra `auth.json` copies are `PANDAY_GROK_AUTH` (colon-separated); extra API
keys are `PANDAY_OPENAI_API_KEYS` (comma-separated) and friends. This is N tokens in
memory, not N CLI installs. A client
talking *to* panday ingress must send the full `provider/model` id
(`OpenAiCompatClient::for_gateway`); stripping `xai/` makes the router honestly refuse
`grok-4.6`.

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

## Operator console (Leptos islands)

The solo gateway serves a real frontend at `GET /` so an operator can see what
booted, whether subscription OAuth loaded, which models route, and try a prompt
without curl. It is **not** the customer billing dashboard (Phase 6) and not
Trunk CSR.

**Architecture.** Leptos 0.8 **islands** on the existing Axum process
(`panday-console`, ssr in the gateway binary, hydrate WASM in the browser):

- Status, providers, OAuth flags, and **callable** models render as HTML on
  the server. They work with WASM disabled. `GET /models` (and `GET /v1/models`)
  ask each signed-in provider what this account can call. The YAML catalog is
  prices, measured context, and pool preference — not the inventory. A new
  Anthropic/OpenAI/xAI model appears when that API lists it; it does not wait
  on a catalog edit.
- The playground is an `#[island]`: only that component hydrates. A form POST
  to `/console/try` is the no-WASM fallback.
- Split/lazy WASM (`cargo leptos --split`, `#[lazy]` islands) is the next
  compile step once more than one island exists. Trunk is rejected: a CSR
  blank page if WASM fails is the wrong failure mode for an operator console.

**Theme.** *Forge* — warm charcoal, one ember accent, 2px radii, system fonts
(no CDN; the air-gap kit cannot fetch Google Fonts). Numbers are mono.
`prefers-color-scheme` is not a mid-page flip: the console is dark.

**Never shown:** access tokens, refresh tokens, raw prompts in the recent-call
list. OAuth is a boolean plus expiry class. "Expired" means the access token
is past `expires_at` at boot — the Max/Pro subscription can still be valid.
Restart the gateway after a CLI login/refresh.

## Also serves: OpenAI-, Anthropic-, and Gemini-compatible ingress

Three inbound dialects, one IR, one router:

| Dialect | Path | Who points here |
|---|---|---|
| OpenAI Chat Completions | `POST /v1/chat/completions` | Grok Build custom model, Codex, Aider, OpenAI SDK |
| Anthropic Messages | `POST /v1/messages` | Claude Code (`ANTHROPIC_BASE_URL`) |
| Gemini generateContent | `POST /v1beta/models/{model}:generateContent` | Antigravity CLI (`GOOGLE_GEMINI_BASE_URL`) — not Gemini CLI |

`GET /v1/models` is OpenAI-shaped. `GET /v1beta/models` is Gemini-shaped. Both list what the signed-in providers actually return.

A client talking *to* panday must send a model the router can place: `provider/model` (`xai/grok-4.6`), or a bare Claude/Gemini id which is qualified (`claude-sonnet-5` → `anthropic/claude-sonnet-5`). `auto` lets policy pick.

The solo gateway on a laptop has no auth: any bearer / `x-api-key` / `x-goog-api-key` is accepted.

Wire fields some upstreams 400 on are stripped on the way out, not rejected inbound:

- Claude 5 / Fable 5 / Mythos: drop `temperature` and `top_p` on the Anthropic wire (`temperature is deprecated`).
- Grok (`grok*`): drop `stop` (`does not support parameter stop`).
- Gemini ingress: drop inbound `stopSequences` so they never become Grok `stop`.

### Pointing agents at a running gateway

Default listen is `127.0.0.1:8080` (`PANDAY_GATEWAY_ADDR`). Examples below use `8088` because this laptop's 8080 is already taken. Subscription OAuth is read from Grok CLI and Claude Code at boot; `GEMINI_API_KEY` on the *gateway* process registers the Gemini *outbound* adapter. That key is independent of the dummy key a client sends *to* us.

**Curl (OpenAI):**

```bash
curl -sS http://127.0.0.1:8088/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer unused' \
  -d '{"model":"xai/grok-4.6","messages":[{"role":"user","content":"reply with the single word pong"}],"max_tokens":64}'
```

**Curl (Anthropic Messages — what Claude Code sends):**

```bash
curl -sS http://127.0.0.1:8088/v1/messages \
  -H 'Content-Type: application/json' \
  -H 'x-api-key: unused' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"claude-sonnet-5","max_tokens":64,"messages":[{"role":"user","content":"reply with the single word pong"}]}'
```

**Claude Code** talks Anthropic Messages. Three things have to be true together:

1. `ANTHROPIC_BASE_URL` is the gateway origin **without** `/v1` — Claude Code appends `/v1/messages`.
2. `ANTHROPIC_API_KEY` is set (any dummy on the solo gateway).
3. The process is launched with `--bare`. Without `--bare`, Claude Code prefers the Max/Keychain subscription and talks to `api.anthropic.com` directly, so panday never sees the call.

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8088
export ANTHROPIC_API_KEY=unused
# optional: ANTHROPIC_MODEL=claude-sonnet-5   # or xai/grok-4.6
claude --bare --print "reply with the single word pong"
```

Proven on this laptop: `--bare --print` returned `pong` for both `ANTHROPIC_MODEL=xai/grok-4.6` and `claude-sonnet-5`. Interactive (no `--bare`) is not the proven path.

`--disallowedTools` consumes subsequent argv as tool names — do not put the prompt after it. A console flag of OAuth "expired" is the *access token* past `expires_at` at gateway boot, not the Max/Pro subscription; restart the gateway after a CLI login/refresh.

**Grok Build** — add *outside* the OpenCodex managed block in `~/.grok/config.toml`:

```toml
[model.panday]
model = "xai/grok-4.6"
base_url = "http://127.0.0.1:8088/v1"
api_backend = "chat_completions"
api_key = "unused"
name = "panday grok-4.6"
```

Then `/model panday`.

**Antigravity CLI (`agy`)** — Gemini `generateContent`, not OpenAI and not Gemini CLI. Install the CLI only (`brew install --cask antigravity-cli` → `agy`), not the desktop app or IDE.

All three of these are required together. `agy` does **not** read `.env` files; the key must be in the process environment (`~/.zshrc`, a wrapper, or the shell you launch `agy` from).

`~/.gemini/antigravity-cli/settings.json`:

```json
{
  "modelProvider": "gemini"
}
```

```bash
export GEMINI_API_KEY=unused
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8088
agy --print "reply with the single word pong"
```

`modelProvider: "gemini"` without `GEMINI_API_KEY` is the error `agy` prints at startup (`modelProvider is set to "gemini" but GEMINI_API_KEY is not set`). That is not a malformed settings file. The dummy `unused` is enough for this solo gateway (no auth). `GOOGLE_GEMINI_BASE_URL` must not include `/v1beta` — `agy` appends `/v1beta/models/{model}:generateContent`.

`agy --model` is ignored: the CLI always sends its Gemini default (`gemini-3.1-pro` / `gemini-3.1-pro-high`). The gateway qualifies that as `gemini/…`. If this process has no Gemini outbound adapter, those names route with `auto` (Grok/Claude OAuth). You cannot pin Grok from agy's flag. `agy --version` does not check `GEMINI_API_KEY`; `--print` and interactive do.

Proven on this laptop: `agy --print` exited 0; traffic hit `POST /v1beta/models/gemini/gemini-3.1-pro:generateContent` and the workhorse (`xai/grok-4.6`) served it. An agentic run may not echo a one-word prompt back — the proof is the request on the gateway, not the string.

**Playground:** `GET http://127.0.0.1:8088/playground` — same Chat Completions path as curl.

**Python OpenAI SDK:**

```python
from openai import OpenAI
c = OpenAI(base_url="http://127.0.0.1:8088/v1", api_key="unused")
print(c.chat.completions.create(
    model="xai/grok-4.6",
    messages=[{"role":"user","content":"pong?"}],
    max_tokens=64,
).choices[0].message.content)
```

Outbound Gemini (this process *calling* Google) is `GEMINI_API_KEY` plus optional `GEMINI_BASE_URL` (default `https://generativelanguage.googleapis.com/v1beta/openai`). That is independent of Antigravity pointing *at* us.

## Non-goals

No business logic (harness owns turns), no transcript storage (log owns it),
no customer billing UI (that's Phase 6). The operator console at `GET /` is
in-process, not a product surface. The gateway is stateless apart from cache
+ circuit state → scales horizontally behind any LB.

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

  **Follow-on: Anthropic Messages and Gemini generateContent.** Claude Code
  posts `POST /v1/messages`; Antigravity CLI (`agy`) posts Gemini
  `generateContent` at `/v1beta/models/{model}:generateContent`. Same IR and
  router as Chat Completions. How to point those CLIs is in §Pointing agents.
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
