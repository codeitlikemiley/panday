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
or `round_robin` (each new request starts one member further). Set at
`GET /accounts` or `PANDAY_ROTATE`. Sticky-per-`session_id` (M25.5),
per-credential breakers, and `UsageRecord.credential_id` are **not** shipped
here — breakers are still `(provider, model)`.

## Remaining % (two numbers)

**Rate-limit headroom** (minutes): response headers, once the transport stops
dropping them (M25.3). OpenAI `x-ratelimit-remaining-*`, Anthropic
`anthropic-ratelimit-*-remaining`, xAI API the OpenAI-shaped set. SuperGrok /
Claude Max / Codex ChatGPT OAuth do **not** document a remaining-percentage API.
We do not scrape their UIs.

**Grant remaining** (the billing window): operator-declared ceiling + counters
from our own usage records. `remaining_pct = 1 - used/ceiling`. Header remaining
overlays the short window when present; the ceiling owns the period. 429 with
remaining 0 → `exhausted` until reset.

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
  (today it is always 0). Success path exposes ratelimit headers.

- **M25.4** `PooledAdapter`: inner loop over credentials. Key A 429 → key B 200;
  both 429 → `RateLimited` so the model chain walks; 400 does not walk keys. ✅
  *(shipped: `panday_gateway::adapters::pool` — live `CredHub` for
  `xai` / `anthropic` / `openai` / `gemini`. Failover or round-robin.
  `GET /accounts` imports Grok CLI, pastes `auth.json`, adds API keys, revokes.
  Env: `PANDAY_*_API_KEYS` comma-separated, `PANDAY_GROK_AUTH` extra files,
  `PANDAY_ROTATE`. Vault rows load at boot when `~/.panday` exists. Mock-proven;
  not live two-account dogfood. Sticky session is M25.5.)*

- **M25.5** Per-credential breakers + sticky `session_id`.

- **M25.6** Operator ceiling, local counters, remaining %, console cards.
  Prometheus `panday_upstream_calls_total{provider,outcome}` only.

- **M25.7** Overlay OpenAI / Anthropic / xAI remaining headers. Missing headers
  leave local counters in charge. OAuth subscriptions without headers stay on
  M25.6 — do not add a scraper.

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
