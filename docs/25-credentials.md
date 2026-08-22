# 25 — Upstream credentials

Operator-owned pool of provider API keys and consumer-subscription OAuth tokens.
The gateway picks which credential spends a call, watches remaining headroom, and
funnels traffic toward whatever still has quota. This is the outbound side of the
only door (ADR-006). Inbound clients still talk to panday; panday talks to the
provider.

It is **not** tenant BYOK. It is **not** the customer `pnd_live_` / `pnd_test_`
table (docs/17): those are hashed because we only ever *verify* them. Upstream
secrets must be *retrieved* to send `Authorization`, so they are envelope-encrypted,
never hashed.

## Three secret kinds (keep them apart)

| Kind | Store | Retrieve? |
|---|---|---|
| Customer `pnd_` keys | sha256, prefix only (docs/17) | No |
| Tool secrets (`GITHUB_TOKEN`) | `SecretVault` (docs/20 T4) | Yes, into sandbox env only |
| **Upstream credentials** | this spec | Yes, into adapter headers only |

## Domain

```
Credential
  id            UUIDv4          — not v7: a secret id must not encode mint time
  provider      xai | anthropic | openai | gemini | …
  kind          api_key | oauth
  label         operator string (console; never a secret)
  last4         last four characters of the secret, stored in the clear
  state         active | exhausted | invalid | revoked
  ceiling       Option<u64>     — operator-declared CALLS per window (M25.6)
  window        Option<Duration>— the window that ceiling applies to (M25.6)
  nonce         24 bytes
  ciphertext    XChaCha20-Poly1305 of the secret
```

AAD for the AEAD is `id || provider || kind`. Ciphertext from one row cannot be
copied onto another.

KEK: 32 bytes. First of `PANDAY_VAULT_KEY` (hex) or `~/.panday/master.key` (raw,
mode `0600`, created on first use). Never logged. Keychain wrap is M25.12.

Laptop store: SQLite `~/.panday/credentials.sqlite`. No plaintext column. A
revoked row keeps `id`/`label`/`last4` so usage history still names it, and the
ciphertext is wiped.

Import is explicit and read-only against official CLI stores (`~/.grok/auth.json`,
Claude Code Keychain, `~/.codex/auth.json`). Never write those files. Never accept
the secret on argv (`ps` sees argv).

## Multiple Grok sessions are N tokens, not N CLIs

Grok CLI itself keeps one session in `~/.grok/auth.json`. That is the official
CLI's store, not panday's limit. The proxy can:

1. Parse **every** `https://auth.x.ai::<client_id>` object in one JSON file
   (two accounts pasted into one `auth.json`).
2. Read extra files listed in `PANDAY_GROK_AUTH` (colon-separated paths) plus
   the default `~/.grok/auth.json`. Missing or empty files are skipped.
3. Hold those access tokens in a live pool (and persist new ones to the sealed
   vault when `~/.panday` is writable).
4. Rotate: same model `xai/grok-4.6`, credential A 429 → credential B.

`panday-gateway` registers a **live pool** per provider (`xai`, `anthropic`,
`openai`, `gemini`). Members are Grok/Claude OAuth tokens and API keys (one
`ANTHROPIC_API_KEY` or several in `PANDAY_ANTHROPIC_API_KEYS`, same for
`OPENAI` / `XAI` / `GEMINI`). The operator console at `GET /accounts` can
import Grok CLI, paste a second `auth.json`, paste API keys, revoke, and pick
**failover** (always try the first key) or **round-robin** (spread requests).
A 400 does not walk keys. If every member 429s the pool returns `RateLimited`
so the model chain can still fail over to Claude.

A second SuperGrok login is a copy of another machine's `auth.json`, not a
second Grok CLI install. Stale tokens are refreshed in memory when a refresh
token is present; a failed refresh drops that token at boot and does not write
the CLI store. This rotation is proven with mocks (`sk-test-aaaa` /
`access-token-a`). It is **not** live two-account dogfood.

## How a call is served

```
router picks a *model* (docs/12)
  → pool picks a *credential* for that model's provider
    → adapter.chat with that secret
```

Inner loop is credentials; outer loop is still the model chain. Every credential
for Grok 429s → `RateLimited` (retryable) → the chain walks to Claude. A 400 does
not walk keys (same rule as model failover, docs/11). Never rotate mid-stream.

**Shipped:** rotation is `failover` (always start at member 0)
or `round_robin` (each new request starts one member further, or one member per
*session* — see M25.5). Set at `GET /accounts` or `PANDAY_ROTATE`. Breakers are
per-credential as well as `(provider, model)`. `UsageRecord.credential_id` is
still **not** shipped.

## Remaining % (two numbers)

**Rate-limit headroom** (minutes): response headers. OpenAI `x-ratelimit-remaining-*`, Anthropic
`anthropic-ratelimit-*-remaining`, xAI API the OpenAI-shaped set. SuperGrok /
Claude Max / Codex ChatGPT OAuth do **not** document a remaining-percentage API.
We do not scrape their UIs.

**Grant remaining** (the billing window): operator-declared ceiling + counters
from our own usage records. `remaining_pct = 1 - used/ceiling`. Header remaining
overlays the short window when present; the ceiling owns the period. 429 with
remaining 0 → `exhausted` until reset.

The ceiling is in **calls**. Not tokens: `UsageRecord` carries no
`credential_id`, so tokens cannot be attributed to a credential yet. Not spend:
a flat-rate seat has no per-call price, so a spend ceiling would be meaningless
for exactly the credentials pooling exists to manage. Calls is also the unit
subscription grants are actually sold in. The window is declared **per
credential**, because a subscription seat resets in hours and an API key in
months, and one global period would be wrong for one of them.

An undeclared ceiling gives `remaining_pct = None`, not 1.0. Unknown is not
"plenty left", and a console showing a full bar for a credential nobody has
measured invites exactly the decision the number exists to inform.

Funnel (M25.8): pick the cred with most remaining; if every cred for a provider
is below threshold, omit that provider from this request's chain.

## Non-goals

- Tenant-uploaded provider keys.
- Scraping `claude.ai` / `grok.com` usage HTML.
- Browser-cookie importers.
- Writing official CLI credential files.
- Putting plaintext in `panday-platform.api_keys`.
- Live provider calls in CI. Mocks until an `#[ignore]` probe the operator runs.

## Testing contract

Fixture secrets are `sk-test-aaaa` / `sk-test-bbbb`. `MockTransport` scripts
status + headers + SSE. No `~/.grok`, no Keychain, no real tokens in CI.
Property: Σ tokens per `credential_id` == Σ usage frames that named it.
A one-credential gateway must keep today's failover behaviour.

## Milestones

- **M25.1** Sealed vault: types, envelope, SQLite, in-memory store. ✅ *(shipped:
  `panday_sdk::vault` — `Kek`, `CredentialStore`, `MemoryStore`, `SqliteStore`.
  Tests: round-trip, wrong KEK, AAD bind, empty file, revoke wipes ciphertext,
  `list` has last4 and no secret, master.key is `0600`.)*

- **M25.2** `panday creds` CLI: add (stdin), `--from-grok|--from-claude|--from-codex`,
  list, revoke. Argv must not accept the token. ✅ *(shipped: `panday creds` in
  `panday-cli`. `add` without `--from-*` reads stdin — trim the pipe, then one
  line. A TTY is refused (`pipe it`); a positional after `add` is rejected so
  `ps` never sees the token. `--from-grok` copies `panday_sdk::oauth::grok_cli()`
  (`~/.grok/auth.json`) as `provider=xai` `kind=oauth`. `--from-claude` copies
  `claude_code()` as `anthropic` oauth. `--from-codex` reads `tokens.access_token`
  from `~/.codex/auth.json` (override `CODEX_HOME` or `PANDAY_CODEX_AUTH`) as
  `openai` oauth. All three are read-only against official CLI stores — two grok
  imports are two rows. `list` prints `id  provider  kind  label  last4  state`,
  never the secret. `revoke <id>` wipes ciphertext via `CredentialStore::revoke`.
  KEK: `PANDAY_VAULT_KEY` else `~/.panday/master.key`. DB: `PANDAY_VAULT_DB` else
  `~/.panday/credentials.sqlite`.)*

- **M25.3** Transport keeps headers. `Retry-After` fills `RateLimited.retry_after_ms`
  (today it is always 0). Success path exposes ratelimit headers. ✅ *(shipped:
  `ResponseHeaders` + `SseResponse` on `HttpStreamTransport::post_sse`. 429
  `Retry-After` is delay-seconds or IMF-fixdate → `RateLimited.retry_after_ms`;
  missing stays 0. Success keeps `x-ratelimit-remaining-*` /
  `anthropic-ratelimit-*-remaining`. A pool or chain of 429s keeps the soonest
  **stated** wait — 0 means "no header", so a silent member abstains from the
  minimum instead of collapsing it. The retry middleware honours a stated wait
  only up to `MAX_HONOURED_RETRY_AFTER` (60s); past that it returns the error
  with the upstream's number intact rather than parking the caller, because that
  sleep sits outside the timeout layer. Overlay onto operator remaining % is
  M25.7; telling a **client** the wait — `Retry-After` on all three ingresses —
  is docs/11 M11.7. An already-elapsed HTTP-date parses to 0 and is therefore
  indistinguishable from "no header", which is the intended reading: an expired
  deadline does mean "you may retry now".)*

- **M25.4** `PooledAdapter`: inner loop over credentials. Key A 429 → key B 200;
  both 429 → `RateLimited` so the model chain walks; 400 does not walk keys. ✅
  *(shipped: `panday_gateway::adapters::pool` — live `CredHub` for
  `xai` / `anthropic` / `openai` / `gemini`. Failover or round-robin.
  `GET /accounts` imports Grok CLI, pastes `auth.json`, adds API keys, revokes.
  Env: `PANDAY_*_API_KEYS` comma-separated, `PANDAY_GROK_AUTH` extra files,
  `PANDAY_ROTATE`. Vault rows load at boot when `~/.panday` exists. Mock-proven;
  not live two-account dogfood. Sticky session is M25.5.)*

- **M25.5** Per-credential breakers + sticky `session_id`. ✅ *(shipped:
  `panday_gateway::circuit::CredentialBreakers`, and session-hashed member
  selection in `PooledAdapter::snapshot`.)*

  **Per-credential breakers.** Before this, one dead key's failures accumulated
  against the `(provider, model)` breaker until it opened — taking every healthy
  sibling in the pool down with it, which is the exact failure pooling exists to
  prevent. Each credential now has its own breaker, keyed by `MemberMeta::id`
  (stable, never the secret). An open credential is *skipped without being
  dialled*; when every one is open the pool returns a **retryable** error so the
  model chain still walks to another provider rather than reporting the caller's
  request as broken.

  The state machine is shared with the route breakers rather than copied — the
  half-open reservation and the probe-decides-alone rule are subtle enough that a
  second implementation would drift. Credential breakers deliberately do **not**
  write docs/21's `circuit_open` gauge: it is labelled by provider, so one dead
  key out of four would report the whole provider as open. Console visibility for
  them is M25.6.

  A 400 does not count against a credential. It is the request's fault, and
  counting it would let one malformed client open every key in the pool for
  everybody else — the same rule that already stops a 400 walking the pool.

  **Sticky sessions.** `round_robin` now picks its starting member from
  `session_id` when the request carries one, instead of advancing the cursor.
  Round-robin exists to spread load across accounts; spreading it *within* one
  conversation gives every turn a cold prompt cache and smears one user's usage
  across subscriptions for no gain. Sticky moves the spreading from per-request
  to per-conversation, which is what was wanted in the first place.

  Two decisions worth keeping:

  - **`failover` ignores the session.** Its contract is "always start at member
    0", and under it the pool is already sticky. Letting a session redefine that
    would quietly change M25.4's meaning, so sticky is a refinement of
    `round_robin` rather than a third `Rotate` mode — no new config surface.
  - **The index comes from the UUID bytes, not `DefaultHasher`.** That hasher's
    output is explicitly not stable across releases, and a sticky choice that
    moves on a toolchain bump is not sticky.

  Sticky is a preference, not a pin: a 429 on the session's credential still
  walks to the next member, and "never rotate mid-stream" is unchanged.

- **M25.6** Operator ceiling, local counters, remaining %, console cards.
  Prometheus `panday_upstream_calls_total{provider,outcome}` only. ✅ *(shipped:
  `pool::Grant` / `MemberUsage`, `CredHub::usage` / `set_grant`, `env_grant`,
  `POST /console/accounts/ceiling`, and the used/remaining columns on
  `GET /accounts`.)*

  **The unit is calls and the window is per credential** — see §Remaining % for
  why tokens and spend both fail here, and why one global period would be wrong
  for either a subscription seat or an API key.

  **Declared in env, overridable in the console.** `PANDAY_<PROVIDER>_CEILING`
  and `PANDAY_<PROVIDER>_WINDOW` (`5h`, `30d`, `90m`, or bare seconds) are read
  at boot and applied to every credential that boot found — env keys, CLI OAuth
  and vault rows alike. A ceiling is *configuration*: it has to survive a
  restart, and the console alone would lose it. Per-credential persistence in
  the sealed vault belongs to **M25.9**, which owns making the vault the boot
  source of truth; adding an unguarded `ALTER TABLE` here would have put a
  schema migration in the wrong milestone.

  **Counters are in-process and reset when the window rolls.** Re-declaring a
  ceiling starts a fresh window, because carrying the old count forward would
  report a percentage of a ceiling that never applied.

  **Only a 429 sets `exhausted`.** Our own count reaching the ceiling means *we*
  think it is spent, which is a guess until the provider agrees — the operator's
  declared number can be wrong in either direction. A rejected request (400)
  spends nothing: it is the caller's fault, the same reason it does not walk the
  pool or count against a breaker.

  **The metric is labelled by provider, never by credential.** Credential ids are
  unbounded in principle — an operator adds and revokes keys all day — so a
  series per credential would make this metric's cardinality a function of how
  often they do. Outcomes are `ok`, `rate_limited`, `error`, `rejected`. The
  per-credential numbers live on `GET /accounts`, where a human reads them and
  retiring one costs nothing.

  **Selection is unchanged.** This milestone measures; it does not yet steer.
  Picking the credential with the most remaining, and omitting a provider whose
  credentials are all below threshold, is **M25.8**.

- **M25.7** Overlay OpenAI / Anthropic / xAI remaining headers. Missing headers
  leave local counters in charge. OAuth subscriptions without headers stay on
  M25.6 — do not add a scraper. ✅ *(shipped: `RemainingSink`, the `limit_*`
  half of `RatelimitRemaining`, `RatelimitRemaining::headroom_pct`, and the
  headroom column on `GET /accounts`.)*

  **M25.3 kept the wrong half of the pair.** It preserved
  `x-ratelimit-remaining-*` and stopped there — but a remainder is not a
  percentage: "412 requests left" says nothing about headroom until you know
  whether the ceiling is 500 or 500,000. The `x-ratelimit-limit-*` /
  `anthropic-ratelimit-*-limit` headers are parsed too, and `headroom_pct` is
  `None` unless both arrived.

  **Headroom is reported as the scarcer of requests and tokens.** A credential
  with 90% of its requests and 3% of its tokens left has 3% of headroom;
  reporting the kinder number would hide the one about to bite.

  **Two numbers, shown separately, not merged.** `remaining_pct` is the
  operator's declared grant over their declared period (M25.6). `headroom_pct`
  is the provider's own short window. They measure different things over
  different timespans, and averaging them would produce a figure that is true of
  neither.

  **The seam is a push, not a getter.** The value appears deep inside the
  adapter, and only the pool knows which *credential* that adapter holds — so
  the pool hands each member a `RemainingSink` when it joins, and the adapter
  reports into it. A `last_remaining()` getter would also race: two concurrent
  calls on one credential would overwrite each other and the reader could not
  tell which answer it received. The sink holds the pool's shared map rather
  than the pool, so there is no reference cycle, and a revoked member's headers
  are dropped with it rather than being inherited by a reused id.

  **Absent stays absent.** SuperGrok, Claude Max and Codex ChatGPT OAuth publish
  no such header. Those credentials report `headroom_pct: None` and their local
  counters keep answering. No scraper, per §Non-goals.

- **M25.8** Selector `most_remaining`. Policy: omit a provider below threshold.

- **M25.9** Gateway boot loads the vault. Env keys become rows if the vault is
  empty (back-compat). OAuth import is "insert if absent", not "the only cred".
  **Partial:** `CredHub::seed_from_process` loads Grok/Claude
  OAuth, `*_API_KEY` + `PANDAY_*_API_KEYS`, then vault rows (skip duplicate
  last4). The console can add more without restart. `panday creds` is M25.2.

- **M25.10** Codex importer proven against the openai adapter, or a note that it
  does not work and skip.

- **M25.11** Hosted Postgres ciphertext (same envelope). Not tenant BYOK.

- **M25.12** Keychain-wrapped KEK. Ask before adding `keyring`. Skip if file+env
  is enough.
