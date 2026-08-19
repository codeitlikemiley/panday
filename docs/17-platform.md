# 17 — panday-platform: accounts, subscriptions, billing

The commercial spine. One metering pipeline serves all three channels
(subscription, API, enterprise) — the difference between them is plan
configuration, not code paths.

## Domain model

```
Account ──< Member (role) >── User (OIDC identities)
Account ──< Workspace ──< Session (log lives with harness; index here)
Account ── Subscription ── Plan ──< Entitlement
Account ──< ApiKey (hashed, scoped, pnd_live_/pnd_test_)
Account ──< LedgerEntry (append-only)          ← the truth
Account ──< CreditGrant (purchases, plan refresh, promos)
```

## Plans & entitlements

Entitlements are typed limits evaluated at the gateway/harness/sandbox edges:

```rust
pub enum Entitlement {
    RequestsPerMin(u32),
    TokensPerDay(u64),
    SpendCeilingMicros(u64),        // hard stop
    ModelPools(Vec<PoolName>),      // e.g. free plan never touches `frontier`
    SandboxTier(SandboxTier),       // free = T1/T2 only; paid = T3 minutes
    SandboxSecondsPerDay(u64),
    ConcurrentSessions(u32),
    SubagentFanout(u8),
    StorageBytes(u64),
    OfflineSeats(u32),              // enterprise
}
```

Sketch tiers (numbers are placeholders to be priced against measured COGS —
the ledger will tell you real per-session cost within a week of dogfooding):
**Free** (local-first: bring-your-own local model, cheap-pool trickle) ·
**Pro** (workhorse pools, generous credits, T3 minutes) · **Max** (frontier
pools, priority routing) · **API** (pure prepaid credits, no UI limits) ·
**Enterprise** (seats + offline distribution + custom pools).

## The ledger (ADR-009)

Append-only, double-entry-flavored:

```sql
CREATE TABLE ledger_entries (
  id            UUID PRIMARY KEY,        -- v7
  account_id    UUID NOT NULL,
  at            TIMESTAMPTZ NOT NULL DEFAULT now(),
  kind          TEXT NOT NULL,           -- usage.model | usage.sandbox | usage.storage
                                         -- | grant.plan | grant.purchase | adjust.refund
  amount_micros BIGINT NOT NULL,         -- negative = consumption (in credit-micros)
  quantity      JSONB NOT NULL,          -- {input_tokens, output_tokens, cache_read, cache_write,
                                         --  model, provider_cost_micros} | {sandbox_ms, tier}
  source        JSONB NOT NULL,          -- {session_id, turn_id, seq, request_id} — replayable
  idempotency_key TEXT UNIQUE NOT NULL   -- request_id-scoped; retries can't double-bill
);
```

- Written **in the request path** by gateway (usage.model) and sandbox
  (usage.sandbox). Fail-closed for API keys, fail-open-with-alarm for our own
  interactive surfaces (a billing outage shouldn't brick paying users
  mid-session; the alarm + backfill job reconciles).
- Balances are a materialized view refreshed transactionally per write;
  entitlement checks read the view (one indexed lookup, no SUM at request
  time).
- **Provable**: `source` points into the event log; a dispute is settled by
  replaying the session and recomputing (03 M3.5 is the property test).

## Credits & pricing mechanics

Everything is denominated in **credit-micros** internally. Model usage
converts via a *price table* versioned in the repo (provider list price ×
margin multiplier per pool; cache_read/cache_write priced at their real
multipliers so the reducer's savings flow to COGS). Subscriptions grant
monthly credits (`grant.plan`); API customers prepay (`grant.purchase`);
overage policy per plan: block (free), throttle-to-cheap-pool (pro), invoice
(enterprise).

## Stripe integration (the projection)

- Subscriptions: Stripe Checkout + customer portal; webhooks
  (`checkout.completed`, `invoice.paid`, `sub.updated`) drive plan state via
  an **idempotent, replayable webhook inbox table** (never trust webhook
  delivery; poll-reconcile nightly).
- Metered overage/API usage: aggregate ledger → **Billing Meters v1**
  (`/v1/billing/meter_events`) hourly, priced per-1M tokens (per-token
  rounding is a known Stripe footgun). Meters are *reporting*; enforcement
  already happened at the edge — Stripe's async aggregation can't do
  real-time stops (ADR-009).
- Stripe v2 "Pricing Plans" native credit burndown is **preview** — watch it,
  don't build on it yet; our grants table already does the job and works
  offline/on-prem where Stripe doesn't exist.
- Fraud posture: trial abuse is a documented plague — free tier gets no T3,
  no frontier pool, hard concurrency caps, and disposable-email/device
  heuristics from day one.

## Auth

OIDC (any IdP; start with GitHub/Google) for humans → short-lived JWTs.
API keys for machines: random 256-bit, stored as argon2 hash, prefix-typed
(`pnd_live_`, `pnd_test_`), scoped (models? sessions? admin?), last-used
tracking, instant revoke. Service-to-service: mTLS or private-network + key,
per deployment shape.

## Enterprise / offline licensing

Signed **entitlement tokens** (ed25519, ~90-day expiry + grace): the offline
daemon (18) validates the signature locally — no phone-home required, renewal
is a file. Seat counting is honest-declaration + audit log; don't build
spyware.

## Admin surface

`panday-platform` serves a minimal internal admin (accounts, grants, refunds,
kill-switch per key) — HTML, boring, behind IdP. Customer dashboard (usage
graphs, keys, invoices) is part of the phase-3 web surface.

## Milestones

- **M17.1** Schema migration set: accounts/keys/plans/ledger; entitlement engine unit-tested against fixture plans. ✅ *(shipped: `migrations/0001_init.sql` + `0002_accounts_keys_plans.sql`, `panday_platform::entitlements`; suites in `tests/entitlements.rs` and the integration lane's `tests/pg_integration.rs`.)*

  **The engine returns three answers, not two.** `Allow`, `Deny`, and `Degrade` — docs/17 says a
  budget stop is "a graceful session pause, not a 500" and docs/12's `budget_soft` demotes to a
  cheaper pool, so an engine that only said yes or no would force every caller to invent the middle
  case and they would each invent it differently. The free plan asking for `frontier` degrades
  rather than failing: a route is a choice among pools, and refusing a request for naming the wrong
  one is pedantry.

  **Order of evaluation is money, then rate, then capability.** The first limit reported is the one
  the user has to act on; telling someone over their spend ceiling that they used the wrong pool
  wastes a support ticket. There is a test that an account over all three limits at once hears
  about the money.

  **A missing measurement is no evidence, not zero.** `Observed` is all `Option`s, because an
  engine that read "usage not looked up" as "usage is zero" allows every request on an account
  nobody measured — the most expensive possible default. It also reads no database and knows nothing
  about time windows: `TokensPerDay` needs *today's* usage, which is a ledger query, and mixing
  "what is allowed" with "what has been spent" makes both untestable.

  **A test walks the free plan's entitlements and requires each one to actually deny at its limit.**
  The failure that catches: a limit added to a plan and never wired into the engine — enforcement
  that exists in the catalogue and nowhere else, which reads as real to everyone who looks at the
  plan.

  **The schema puts invariants in the database rather than in handlers.** One active subscription
  per account is a partial unique index ("two active plans" is a state nobody wrote a handler for);
  a grant must be positive (a "grant" that takes credit away is an adjustment, and calling it a
  grant makes the two indistinguishable in a report); API keys store a **hash only**, so a stolen
  database is not a stolen key, and revocation is a timestamp because an audit that cannot show a
  key *was* revoked cannot show when.

  `plans` moved to the lint's `GLOBAL_TABLES` when the table was actually written: it had been
  guessed at as tenant-scoped, and a plan is a catalogue row that means the same thing for every
  account — the account's relationship to it lives in `subscriptions`, which is scoped.
- **M17.2** Ledger write path from gateway+sandbox with idempotency; balance view; property test vs event-log replay.
- **M17.3** API keys end-to-end (issue, scope, revoke) securing the OpenAI-compat ingress; per-key rate limiting.
- **M17.4** Stripe checkout+webhooks inbox+nightly reconcile in test mode; plan grants land as ledger entries.
- **M17.5** Meter export job (hourly aggregates → Billing Meters); invoice sanity check vs ledger to the cent on a seeded month.
- **M17.6** Entitlement tokens for offline; `panday local` honors + expires them.
- **M17.7** Admin panel + abuse guardrails (velocity checks, disposable-email list).
