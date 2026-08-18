# 17 — ferrum-platform: accounts, subscriptions, billing

The commercial spine. One metering pipeline serves all three channels
(subscription, API, enterprise) — the difference between them is plan
configuration, not code paths.

## Domain model

```
Account ──< Member (role) >── User (OIDC identities)
Account ──< Workspace ──< Session (log lives with harness; index here)
Account ── Subscription ── Plan ──< Entitlement
Account ──< ApiKey (hashed, scoped, frm_live_/frm_test_)
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
(`frm_live_`, `frm_test_`), scoped (models? sessions? admin?), last-used
tracking, instant revoke. Service-to-service: mTLS or private-network + key,
per deployment shape.

## Enterprise / offline licensing

Signed **entitlement tokens** (ed25519, ~90-day expiry + grace): the offline
daemon (18) validates the signature locally — no phone-home required, renewal
is a file. Seat counting is honest-declaration + audit log; don't build
spyware.

## Admin surface

`ferrum-platform` serves a minimal internal admin (accounts, grants, refunds,
kill-switch per key) — HTML, boring, behind IdP. Customer dashboard (usage
graphs, keys, invoices) is part of the phase-3 web surface.

## Milestones

- **M17.1** Schema migration set: accounts/keys/plans/ledger; entitlement engine unit-tested against fixture plans.
- **M17.2** Ledger write path from gateway+sandbox with idempotency; balance view; property test vs event-log replay.
- **M17.3** API keys end-to-end (issue, scope, revoke) securing the OpenAI-compat ingress; per-key rate limiting.
- **M17.4** Stripe checkout+webhooks inbox+nightly reconcile in test mode; plan grants land as ledger entries.
- **M17.5** Meter export job (hourly aggregates → Billing Meters); invoice sanity check vs ledger to the cent on a seeded month.
- **M17.6** Entitlement tokens for offline; `ferrum local` honors + expires them.
- **M17.7** Admin panel + abuse guardrails (velocity checks, disposable-email list).
