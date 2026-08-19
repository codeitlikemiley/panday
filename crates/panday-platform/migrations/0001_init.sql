-- The first migration (docs/02 M2.3, extended by docs/17 M17.1).
--
-- Two tables, chosen because they are the two the rest of the platform hangs off: an account
-- to own things, and the append-only ledger that is the source of truth for money (ADR-009).
-- Everything else — keys, plans, grants — arrives at M17.1 against this same account.
--
-- `account_id` is on every tenant table by construction (docs/20 T5), and the M20.3 lint reads
-- this file to check it.

CREATE TABLE IF NOT EXISTS accounts (
    account_id  UUID PRIMARY KEY,
    -- The display name, not an identity: authentication is M17.3's problem and an email column
    -- here would be a login system nobody designed.
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- docs/17 §the ledger, verbatim in shape. Append-only: there is no UPDATE or DELETE path in the
-- code, and a correction is a new entry (ADR-009), because a ledger you can edit is a ledger
-- nobody can be shown.
CREATE TABLE IF NOT EXISTS ledger_entries (
    id              UUID PRIMARY KEY,
    account_id      UUID NOT NULL REFERENCES accounts(account_id),
    at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- usage.model | usage.sandbox | usage.storage | grant.plan | grant.purchase | adjust.refund
    kind            TEXT NOT NULL,
    -- Negative = consumption, in credit-micros. Integers, because money summed thousands of
    -- times per session must not accumulate binary rounding error.
    amount_micros   BIGINT NOT NULL,
    -- {input_tokens, output_tokens, cache_read, cache_write, model, provider_cost_micros}
    -- or {sandbox_ms, tier}. JSONB rather than columns: the shape differs per kind, and a
    -- column per kind would make every new metered good a migration.
    quantity        JSONB NOT NULL,
    -- {session_id, turn_id, seq, request_id} — points into the event log, which is what makes
    -- a dispute settleable by replay (docs/17, M3.5).
    source          JSONB NOT NULL,
    -- Request-scoped, so a retry cannot double-bill. UNIQUE is the enforcement; the code does
    -- not get a vote.
    idempotency_key TEXT NOT NULL UNIQUE
);

-- Balances are read on every entitlement check, so the lookup must be one index hit rather
-- than a SUM over the account's history (docs/17).
CREATE INDEX IF NOT EXISTS ledger_entries_account_at
    ON ledger_entries (account_id, at DESC);
