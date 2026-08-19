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
API keys for machines: random 256-bit, stored as a **sha256** hash (amended
from argon2, see M17.3), prefix-typed (`pnd_live_`, `pnd_test_`), scoped
(models? sessions? admin?), last-used tracking, instant revoke.
Service-to-service: mTLS or private-network + key, per deployment shape.

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
- **M17.2** Ledger write path from gateway+sandbox with idempotency; balance view; property test vs event-log replay. ✅ *(the gateway path and idempotency at M11.4, the sandbox path at M14.7, the property test at M3.5 — 200 generated sessions, zero discrepancy — and the balance view here: `migrations/0003_balances.sql`, `pg::{balance_micros, balance_from_entries, balance_drift, repair_balance}`.)*

  **A summary table, not a Postgres `MATERIALIZED VIEW`.** A real materialized view is refreshed by a
  command, which is either periodic — so the balance is stale exactly when someone is spending fast —
  or per-write, which locks the whole view. A row updated in the same transaction as its ledger entry
  is what "refreshed transactionally per write" has to mean in practice, and reading it is one index
  hit instead of the `SUM` docs/17 explicitly does not want at request time.

  The transaction also makes idempotency free: on a duplicate the insert fails, everything rolls back,
  and the balance never moved. A write path that updated the balance outside the transaction would
  double-count exactly the retries idempotency exists to absorb.

  **A destructive migration, caught by the integration lane.** The backfill was written `ON CONFLICT
  DO UPDATE`, which is not idempotent but *destructive*: it overwrites every balance with a snapshot
  taken at that instant, and migrations run on every boot — so a rolling deploy would stomp live
  balances mid-traffic. The suite found it because several tests each migrate while another is
  appending, and balances came back short by whatever had been written in between. It is `DO NOTHING`
  now. The lesson generalises: the advisory lock (M2.3) serialises migrations against *each other*,
  and nothing serialises them against application traffic — a migration has to be safe to run while
  the system is working.

  **A drift check needs a repair.** `balance_drift` finds accounts whose cached total disagrees with
  their entries (one statement, not a loop per account — a drift check that takes a minute per account
  is one nobody schedules), and `repair_balance` recomputes one account. A monitor that only reports
  leaves an operator hand-writing `UPDATE`s against a money table at 3am, which is how a drift becomes
  a bigger drift. M21.4's monitor is what will call these on a schedule.
- **M17.3** API keys end-to-end (issue, scope, revoke) securing the OpenAI-compat ingress; per-key rate limiting. ✅

  **Amended: sha256, not argon2.** Argon2 exists to make guessing a *low-entropy* secret expensive —
  a password. A key here is 244 bits of `Uuid::new_v4()` randomness; there is no dictionary to run
  and no brute-force surface for a work factor to defend. What argon2 would add is ~100ms of CPU on
  the authentication path of every request, and the predictable consequence of that is a verification
  cache — which is a second copy of the credential store, with its own invalidation bug, standing
  between `revoke` and the request it is meant to stop. sha256 keeps revocation instant and the auth
  path a single indexed lookup. This holds *only* because the key is machine-generated; the day a
  user-chosen secret enters this table, it needs argon2 and its own column.

  **One 401 for every failure.** Missing, malformed, unknown and revoked all return the same body.
  Distinguishing them tells somebody holding a token they found in a log whether it was ever real,
  which is exactly the fact worth having. The one exception is a *scope* failure, which is named:
  the holder already proved they have the key, so the only thing left to tell them is which door it
  does not open.

  **The plaintext is returned once and is not recoverable.** `issue` is the only function that ever
  sees it; the table holds a hash, and `list` returns no key material at all — not even a truncated
  form, which would make every audit log a partial leak. A key you can retrieve is a key an attacker
  can retrieve.

  **Rate limiting is per process, deliberately.** A shared limiter needs Redis or a database round
  trip on every request; neither is in docs/02's dependency table, and both cost more than the thing
  they bound. With N gateway instances the effective limit is N×, which is stated here rather than
  discovered later — the right trade until the deployment shape that needs a shared counter exists
  (docs/22 shape 3). The limit is checked *after* authentication, or an unauthenticated flood would
  consume the budget of whatever key it guesses at and turn the limiter into the denial of service
  it exists to prevent.

  **The ingress default is still no auth.** `NoAuth` accepts everything as one account, and that is
  what `panday local`, a solo gateway on a laptop and every M11.5 test wire. An ingress that demanded
  a key before accounts exist would make the offline tier (ADR-011) impossible. `panday-platform`
  supplies the real `Authenticator`, so the gateway never links Postgres.
- **M17.4** Stripe checkout+webhooks inbox+nightly reconcile in test mode; plan grants land as ledger entries. ✅ *(shipped: `panday_platform::billing` + migration `0007_billing_inbox.sql`, `POST /v1/billing/webhook`, `panday-platform billing apply|stuck`. **Stripe's own API is not called** — see below.)*

  **Receiving and applying are separate steps over a durable row.** A handler that applies an effect
  and then returns 200 has three ways to be wrong — the effect applied twice, the 200 lost, events
  out of order — and all three disappear when the inbox is keyed by Stripe's own event id. A
  redelivery is a no-op decided by the primary key, not by whichever code path happens to run.

  **Nothing here calls Stripe, and that is the design rather than a gap.** docs/17 already says
  "Meters are *reporting*; enforcement already happened at the edge". Plan state, grants and the
  ledger live in our database; Stripe is a system we tell and a system that tells us about payments.
  What could not be built without a live account is the signature check and the HTTP client — the
  webhook endpoint takes a shared secret instead, compared in constant time, and refuses to mount at
  all unless one is configured. A half-implemented signature check would be worse than an honest
  shared secret, because it looks like the real thing.

  **A grant's effect is a ledger entry keyed by the Stripe event id.** So replaying the whole inbox
  — which an operator will do — cannot double-credit, and the balance stays a sum over one table.

  **Nothing is dropped to keep the queue moving.** A malformed event or an unknown customer stays
  in the table with its reason and an attempt count; the batch is ordered *fewest attempts first*,
  because ordering by age alone lets a wall of permanently-broken events starve a paying customer's
  checkout behind a finite batch limit. That ordering exists because a test wrote twenty unappliable
  events and then a good one.
- **M17.5** Meter export job (hourly aggregates → Billing Meters); invoice sanity check vs ledger to the cent on a seeded month. ✅ *(shipped: `billing::export_hour`, `billing::check_invoice`, `meter_exports` cursor, `panday-platform billing export <hours-ago>`. **The sink is a trait** — the shipped implementation records rather than sends.)*

  **The cursor is ours because Stripe's aggregation is asynchronous** and cannot deduplicate for us
  (ADR-009). `meter_exports` is keyed `(account, hour, meter)`, so re-running an hour is a
  primary-key collision rather than a second charge — and the row is claimed *before* the send and
  released if the send fails, because a cursor that advances on a failure silently drops an hour of
  somebody's usage and nothing downstream ever notices: the invoice is simply smaller.

  **One account's failure does not abandon the hour.** The first version returned on the first
  error, which meant a single bad customer record stopped every other account from being reported
  and made the retry re-walk the whole hour to reach the same failure. Failures are counted, their
  cursors released, and the caller decides what a non-zero count means.

  **Whole tokens, priced per million on Stripe's side.** docs/17 flags per-token rounding as a known
  footgun, and it is: a fractional unit price rounded per event loses a percent of a bill in a way
  nobody can reconstruct afterwards. Rounding happens once, on a number both sides can see.

  **The sanity check compares what we reported against the ledger**, not against an invoice PDF —
  the report is the number the invoice is computed from, so a difference is a bug we can fix before
  a customer sees it. It is signed, and there is a test where it *fails*, because a check that
  cannot fail is decoration.
- **M17.6** Entitlement tokens for offline; `panday local` honors + expires them. ✅ *(shipped: `panday_plugins::entitlement`, `panday-platform entitle …`, `panday local --entitlement <file>`.)*

  **Expiry degrades; it does not brick.** Past the grace window the token stops granting and the
  software keeps working at the community tier. Bricking a paying customer's laptop over a renewal
  e-mail is not a business model, it is an outage you charged for — and ADR-011 says the offline
  tier needs no account at all, so there is a complete product to fall back to.

  **Verification is local, always.** No revocation check, no activation, no call home. A licence
  that stops working because a network is down fails exactly when the offline tier is most
  valuable.

  **Seats are declared, never counted.** The token carries the number that was bought. Nothing
  counts machines, fingerprints hardware or phones home, and there is a test asserting so — the day
  something starts counting, that test is what has to be deleted, which is the point of writing it.
  docs/17 calls the alternative spyware.

  **A wrong file is an error; an expired one is not.** A typo in a path or a key silently
  downgrading a paying customer to the community tier is a support ticket that takes a week to
  reach the truth, so a missing or tampered licence stops the boot. An expired licence boots and
  says so, every run — a warning that appears once, on the day it expires, is one nobody sees.

  **A day of clock skew is tolerated.** An air-gapped box with a dead RTC is a real thing, and
  refusing a valid licence because a laptop thinks it is Tuesday gets a product ripped out. Moving
  a clock back to extend a licence works with or without that tolerance, which is the honest reason
  expiry is a business control rather than a security one.

  **Issuing needs no database.** A licence is a statement about a contract, and an air-gapped
  customer may never have had an account (docs/18 M18.7); tying the one artifact that must work
  offline to the one component that cannot would be backwards.
- **M17.7** Admin panel + abuse guardrails (velocity checks, disposable-email list). ✅ *(shipped: `panday_platform::admin` at `/admin`, plus the guardrails in `panday_platform::abuse` — see docs/20 M20.4.)*

  **HTML, and boring on purpose.** No JavaScript, no build step, no framework: this is a page an
  operator opens at 3am on whatever browser is on the machine they are logged into, and a
  single-page app with a build pipeline is a thing that can break on the day you need it. The
  stylesheet is inline, because a separate asset is a second request that can 404.

  **Behind an `admin`-scoped key, not yet behind an IdP.** docs/17 asks for an IdP and this repo has
  none to integrate with. The scope check is the honest interim and a real control rather than a
  placeholder: an admin key is minted deliberately, revoked in one command, and its id lands in
  every audit row so "who did this" has an answer. A key that authenticates but lacks the scope gets
  the same refusal as one that does not authenticate at all — learning that it *almost* worked is
  learning that it is close.

  **Every value on the page is escaped.** Account names are customer-chosen, and an admin page that
  renders one unescaped is a stored XSS aimed at the single session with admin scope.

  **The kill switch exists as a command as well as a page**, because the moment you need it most is
  the moment something else is already on fire and a browser is the wrong tool.
