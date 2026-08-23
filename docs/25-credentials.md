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
  provider      xai | anthropic | openai | gemini | codesandbox | …
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

KEK: 32 bytes, resolved by `Kek::resolve` in this order — `PANDAY_VAULT_KEY`
(hex), then an existing `~/.panday/master.key` (raw, mode `0600`), then the
macOS Keychain **if** `PANDAY_VAULT_KEYCHAIN=1`, else a freshly created file.
Never logged. The file beats the keychain deliberately: see M25.12.

Laptop store: SQLite `~/.panday/credentials.sqlite`. No plaintext column. A
revoked row keeps `id`/`label`/`last4` so usage history still names it, and the
ciphertext is wiped.

Import is explicit and read-only against official CLI stores (`~/.grok/auth.json`,
Claude Code Keychain, `~/.codex/auth.json`). Never write those files. Never accept
the secret on argv (`ps` sees argv).

**CodeSandbox workspace tokens** (`provider=codesandbox`, env `CSB_API_KEY`) are
the same store for a different job: T3-remote (docs/14 M14.9). Paste a token
from https://codesandbox.io/t/api; sandboxes and VM credits are billed to
*that* workspace. Fail-closed without one. An operator may register more than
one token they already own (failover, like extra OpenAI keys). The product
does not farm free accounts or rotate burner logins — each token is that
user's/workspace's own plan (CodeSandbox ToS 2.3 / 4.4(l)).

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
Property: Σ tokens per `credential_id` == Σ usage frames that named it —
**pending, and not currently enforceable**: `UsageRecord` carries no
`credential_id` (see §How a call is served), so there is nothing for this to
range over. It is stated here as the contract the field must satisfy when it
lands, not as a guard that exists.
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

- **M25.8** Selector `most_remaining`. Policy: omit a provider below threshold. ✅
  *(shipped: `Rotate::MostRemaining`, `PooledAdapter::all_below_threshold`,
  `ProviderAdapter::is_exhausted`, and the chain skip in `Gateway::chat`.)*

  The first milestone where these numbers **steer traffic** rather than describe
  it, which is why the two safeguards below matter more than the selector.

  **"Most remaining" is the scarcer of the two numbers.** The operator's grant
  (M25.6) and the provider's short-window headroom (M25.7) measure different
  things, and a credential with a fat monthly grant and a nearly-spent minute
  window is precisely the one about to 429. Choosing on the kinder figure would
  reliably pick the credential most likely to fail — the same reasoning that
  makes headroom itself the scarcer of requests and tokens.

  **Ranking uses only what is measured; unknown sorts last.** Placing an
  unmeasured credential *between* two known values would mean inventing a number
  for it, and this spec refuses to invent numbers everywhere else. The sort is
  stable, so unmeasured credentials keep their configured order among
  themselves — a pool of OAuth seats with no ceiling and no headers, which is
  the normal case, behaves exactly as it did before.

  The consequence, stated rather than hidden: a credential known to be at 2% is
  still tried before one nobody has measured, and will probably 429 first.
  Ordering is not the tool for that. The **threshold** is, and it removes the
  credential from the chain rather than reshuffling it — two mechanisms, one job
  each. An operator who wants the nearly-spent one skipped sets a threshold; one
  who has not expressed an opinion about "low" gets the credential we at least
  know has something left.

  **The funnel is opt-in.** `PANDAY_REMAINING_THRESHOLD` defaults to `0.0`,
  meaning omit nothing. M25.6 established that an operator's declared ceiling is
  an estimate and only a 429 proves a credential is spent; omitting a provider
  from a request's chain on the strength of a guess would turn a wrong estimate
  into an outage. An operator opts in with `PANDAY_REMAINING_THRESHOLD=0.05`.

  **Only *known* credentials can be below threshold.** `all_below_threshold`
  ignores unmeasured ones entirely, so no threshold — however aggressive — can
  funnel out a pool nobody has measured.

  **An omitted provider is skipped, not failed.** The check runs before the
  adapter is dialled, in the same place and the same shape as the open-breaker
  skip, so the route audit records it as a leg that was not attempted and the
  chain walks on. The metric outcome is `exhausted`.

- **M25.9** Gateway boot loads the vault. Env keys become rows if the vault is
  empty (back-compat). OAuth import is "insert if absent", not "the only cred". ✅
  *(shipped: the `ceiling`/`window_secs` columns and their migration,
  `CredentialStore::set_grant`, empty-vault seeding in
  `CredHub::seed_from_process`, and `creds::persist_grant` behind the console
  form.)*

  **Seeding happens only when the vault is empty.** Adopting whatever env and
  the CLI stores supplied is what lets the *next* boot find those credentials
  even if the variable is gone. Doing it to a populated vault would resurrect a
  credential the operator had revoked, every time they restarted with a stale
  variable still exported.

  **A vault row's ceiling beats the env default.** `PANDAY_<PROVIDER>_CEILING`
  declares one number for a whole provider; a row declares one for *this*
  credential. The specific statement wins, and it is also the one the operator
  made most recently and most deliberately — on the console, against a
  credential they were looking at.

  **Grants are matched by `(provider, last4)`, not by member id.** Pool member
  ids are regenerated on every boot, so an id written today matches nothing
  tomorrow. `last4` is stored in the clear by design, stable for the life of the
  secret, and unique within a provider because the pool refuses a duplicate.

  **The migration checks before it alters.** `PRAGMA table_info`, then add what
  is missing — not `ALTER TABLE` with the error swallowed, which cannot tell
  "column already exists" from "the disk went read-only", and a vault that
  silently half-migrates is worse than one that refuses to open. This is the
  repo's first schema migration and the pattern the next one should copy.

  **Existing rows are untouched.** The AEAD's AAD is `id || provider || kind`
  and neither new column is part of it, so every row still decrypts and nothing
  is resealed. `set_grant` is deliberately separate from `put` for the same
  reason: editing a ceiling must not handle the secret at all.

  Persistence is best-effort. A laptop with no writable `~/.panday` still gets
  the in-process grant; it just does not survive a restart, exactly as before.
  `panday creds` is M25.2.

- **M25.10** Codex importer proven against the openai adapter, or a note that it
  does not work and skip. ✅ *(proven at the auth layer; blocked at billing.
  Probe: `cargo test -p panday-cli --test codex_probe -- --ignored`.)*

  **The token authenticates.** Posting a Codex `tokens.access_token` to
  `api.openai.com/v1/chat/completions` as a plain `Bearer` returned **HTTP 429
  `insufficient_quota` / `credit_balance_exhausted`** — not 401. The control
  matters: a garbage token on the same endpoint returns **401
  `invalid_api_key`**. OpenAI accepted the credential and refused on money. So
  `panday creds add --from-codex` produces something the `openai_compat` adapter
  can genuinely use, and the importer needs no change.
  *(Measured 2026-08-22 against a real `~/.codex/auth.json`.)*

  **What it cannot do is pay.** That login had `auth_mode: chatgpt` and a null
  `OPENAI_API_KEY` — a ChatGPT subscription, which does not come with API
  credit. Codex CLI itself does not spend API credit: it talks to the ChatGPT
  backend, which is why the file also carries an `account_id`. Importing the
  token gives the gateway a credential that authenticates and then cannot buy a
  completion until that account has API billing of its own.

  This is why the milestone is *proven* rather than *skipped*, and the
  distinction is worth keeping straight: the importer was never the problem, and
  a future reader who sees 429s from an imported Codex credential should look at
  the billing page, not at this code.

  **Nothing special is needed to survive it.** An out-of-credit credential fails
  like any other: the pool walks to a sibling, the per-credential breaker
  (M25.5) counts the failures and stops dialling it, and the model chain moves
  on. Mapping `insufficient_quota` to something other than `RateLimited` was
  considered and not done — the observable behaviour is already correct, and a
  new error shape would have to justify itself against docs/10's vocabulary
  rather than against one provider's error string.

  The probe is `#[ignore]`d per §Testing contract and asserts only the narrow
  thing it can: that the response is not a 401. What the account can afford
  afterwards is a fact about the account, not about the importer.

- **M25.11** Hosted Postgres ciphertext (same envelope). Not tenant BYOK. ✅
  *(shipped: `panday_sdk::vault::PgStore`,
  `crates/panday-platform/migrations/0009_credentials.sql`, and the conformance
  suite below run against a real Postgres in the integration lane.)*

  **Same envelope, literally.** `PgStore` reuses `seal`/`open`/`validate_put`
  and the AAD (`id || provider || kind`) unchanged — the only differences from
  `SqliteStore` are the ones Postgres forces: `$1` placeholders, `bytea`, and a
  real `uuid` column. A row is the same bytes under the same key wherever it is
  stored, which is what makes this a *store* rather than a second format.

  **A conformance suite came first, and it is the reason this was cheap.**
  `panday_sdk::vault::conformance::run` is one matrix of nine invariants that
  every `CredentialStore` must satisfy. Before it, `MemoryStore` and
  `SqliteStore` had *disjoint* test sets — only one was ever asked whether it
  rejected a duplicate id, only the other whether a grant round-tripped — so
  each was trusted for something it had never been tested for. A trait with two
  implementations and two disjoint test sets has an unknown contract; adding a
  third to that would have been guesswork. Both existing stores passed
  unmodified, so the suite found no divergence — but it is now impossible for a
  fourth store to pass by testing only what it happens to do.

  **Rows are operator-global, and the migration says so.** Every other table in
  `panday-platform` carries `account_id NOT NULL REFERENCES accounts`; this one
  does not. That is the decision, not an oversight: these are the operator's
  upstream credentials, spent on everyone's behalf, so there is no tenant that
  could own them — and scoping them would imply customers may supply their own,
  which §Non-goals rules out first. The migration carries an explicit
  `-- tenant-scoping: DELIBERATELY NOT SCOPED` block, because the next reader
  will otherwise assume the convention was forgotten.

  **KEK provisioning on a hosted deployment — `PANDAY_VAULT_KEY`, and there is
  no second option.** `Kek::resolve`'s remaining sources are a laptop's:
  `~/.panday/master.key` does not exist in a container and the macOS Keychain
  does not exist on Linux. Falling through to the last step would *generate* a
  key, which on a hosted node means every restart mints a KEK that cannot read
  the rows the previous one wrote — the failure looks like data corruption and
  is not. So:

  - Set `PANDAY_VAULT_KEY` to the 64-character hex of a 32-byte key, from the
    deployment's own secret manager. It is never a database row, never a
    committed file, and never in the same blast radius as the ciphertext it
    unlocks — a KEK stored beside the vault is not a KEK.
  - **Back it up before the first `put`, not after.** M25.12's warning applies
    with more force here: a lost KEK is a permanently unreadable vault, and a
    hosted one holds every credential the fleet runs on. There is no recovery
    path and there is not meant to be.
  - Rotating it is re-encryption, not a config change: read every row with the
    old key, `put` it under the new one. Nothing here does that yet, and
    pretending otherwise by making the variable a list would be worse.

  The table lives in the platform migrations rather than being created on
  connect, unlike `SqliteStore`: a laptop file has no deployer, a hosted
  database does, and a process that migrates whatever database it happens to
  open is a process that migrates the wrong one eventually.


- **M25.12** Keychain-wrapped KEK. Ask before adding `keyring`. Skip if file+env
  is enough. ✅ *(shipped: `Kek::resolve`, `PANDAY_VAULT_KEYCHAIN`, and a
  macOS-only `keyring` dependency. `keyring` approved by the user 2026-08-22 per
  CLAUDE.md §5.)*

  **Precedence, and the order matters more than anything else here — a lost KEK
  is an unreadable vault, permanently:**

  1. `PANDAY_VAULT_KEY`. An explicit override wins; that is what it is for.
  2. **An existing `~/.panday/master.key`, before the keychain, always.** If a
     vault was sealed under the file's key, preferring a keychain entry would
     hand back a *different* key and make every row undecryptable. `resolve`
     never consumes or deletes the file it read — migration is something an
     operator does deliberately, not something a library does on boot.
  3. The keychain, **only** when `PANDAY_VAULT_KEYCHAIN=1`.
  4. Otherwise generate, and store wherever step 3 decided.

  **The opt-in is not timidity.** `keyring` can fall back to an in-memory store
  when no backend is present. That store reads back correctly inside one process
  and is empty at the next boot — so a silent default would lose vaults on
  exactly the machines least able to notice, and no same-process check can catch
  it. An operator who asks for the keychain gets a loud error when it is
  unavailable; one who does not ask is never exposed to it.

  **Writes are verified through a fresh entry.** If the keychain accepts the KEK
  and then will not return it, `resolve` refuses rather than sealing a vault
  against a key that may not survive a restart.

  **macOS only, at the dependency level.** `keyring` is a
  `cfg(target_os = "macos")` dependency, so it is not compiled elsewhere at all
  and a Linux build cannot reach a mock store even by accident. The `keychain`
  feature of `apple-native-keyring-store` is named directly because `keyring`
  enables that backend without choosing a store, and the crate refuses to build
  without one; `protected` is the data-protection variant and needs an
  entitlement we do not have.

  **The cost, recorded rather than discovered later.** `keyring` adds **41
  components to `sbom.cdx.json`** — an entire D-Bus/secret-service stack
  (`zbus`, `zvariant`, `secret-service`, `async-executor`), AES/HKDF/HMAC
  crates, and `uds_windows`. **None of them compile here:** `cargo tree` shows
  the Apple store as the only backend built. They appear because `Cargo.lock`
  records optional dependencies regardless of which features are enabled, and
  the SBOM is generated from the lockfile. `cargo deny` is clean on all of it.
  Accepted deliberately (2026-08-22) — the alternative is an SBOM generator that
  filters by compiled target, which is a different piece of work and changes how
  every future SBOM reads.

  Migration from an existing file is deliberately **not** automatic: read the
  hex out of `master.key`, set it as `PANDAY_VAULT_KEY` once to confirm the
  vault opens, then store it in the keychain and move the file aside. Doing that
  silently on boot is how people lose vaults.
