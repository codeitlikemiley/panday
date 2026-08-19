-- M17.1: the rest of the account model (docs/17 §domain model).
--
-- Four tables, and every one of them carries `account_id` (docs/20 T5). `plans` is the
-- exception that proves the rule: a plan is a *catalogue* row, the same for everyone, so it is
-- on `tenancy::GLOBAL_TABLES` and has no account column. Making that explicit is the point —
-- a table with no tenant column is a claim someone made, not an omission.

-- API keys. **Hashes only.** A stolen database must not be a stolen key: the plaintext exists
-- once, in the response to the create call, and is never stored (docs/17 §API keys).
CREATE TABLE IF NOT EXISTS api_keys (
    key_id       UUID PRIMARY KEY,
    account_id   UUID NOT NULL REFERENCES accounts(account_id),
    -- sha256 of the key. Unique so a lookup is one index hit — the auth path runs on every
    -- request and cannot afford a scan.
    key_hash     TEXT NOT NULL UNIQUE,
    -- `pnd_live_` / `pnd_test_`. Stored so a listing can show which environment a key belongs to
    -- without holding the key.
    prefix       TEXT NOT NULL,
    name         TEXT NOT NULL,
    -- Scopes as a JSON array: they are a set that grows, and a column per scope would make every
    -- new capability a migration.
    scopes       JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Revocation is a timestamp, not a delete: an audit that cannot show a key *was* revoked
    -- cannot show when.
    revoked_at   TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS api_keys_account ON api_keys (account_id);

-- The plan catalogue. Global: the same plan means the same thing for everyone, and an account's
-- relationship to it lives in `subscriptions`.
CREATE TABLE IF NOT EXISTS plans (
    plan_id      TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    -- The entitlement set, as the typed enum serialises (docs/17 §plans & entitlements). JSONB
    -- because entitlements are a versioned list and a column per limit would make every new
    -- limit a migration — and because the *engine* is what evaluates them, not SQL.
    entitlements JSONB NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Which plan an account is on, and since when.
--
-- One active subscription per account, enforced by a partial unique index rather than by
-- application code: "two active plans" is a state nobody wrote a handler for, and the database
-- can simply refuse it.
CREATE TABLE IF NOT EXISTS subscriptions (
    subscription_id UUID PRIMARY KEY,
    account_id      UUID NOT NULL REFERENCES accounts(account_id),
    plan_id         TEXT NOT NULL REFERENCES plans(plan_id),
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Absent = active. A cancellation is an end date, so history survives.
    ended_at        TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS subscriptions_one_active
    ON subscriptions (account_id)
    WHERE ended_at IS NULL;

-- Credit grants (docs/17 §domain model). Kept alongside the ledger rather than inside it because
-- a grant has a lifetime — it can expire — while a ledger entry never changes. The *effect* of a
-- grant is still a ledger entry, so the balance stays a sum over one table.
CREATE TABLE IF NOT EXISTS credit_grants (
    grant_id     UUID PRIMARY KEY,
    account_id   UUID NOT NULL REFERENCES accounts(account_id),
    -- plan.refresh | purchase | promo | adjustment
    reason       TEXT NOT NULL,
    amount_micros BIGINT NOT NULL CHECK (amount_micros > 0),
    granted_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ,
    -- The ledger entry this grant produced, so the two can be reconciled without guessing.
    ledger_entry_id UUID REFERENCES ledger_entries(id)
);

CREATE INDEX IF NOT EXISTS credit_grants_account ON credit_grants (account_id);
