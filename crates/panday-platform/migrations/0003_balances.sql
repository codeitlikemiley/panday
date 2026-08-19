-- M17.2: the balance view (docs/17 §the ledger).
--
-- > "Balances are a materialized view refreshed transactionally per write; entitlement checks read
-- > the view (one indexed lookup, no SUM at request time)."
--
-- A summary *table* rather than a Postgres MATERIALIZED VIEW, because a real materialized view is
-- refreshed by a command — `REFRESH MATERIALIZED VIEW` — which is either periodic (so the balance is
-- stale exactly when someone is spending fast) or per-write (which locks the whole view). A row
-- updated in the same transaction as its ledger entry is what "refreshed transactionally per write"
-- has to mean in practice, and it is one index hit to read.
--
-- The invariant — `balances.balance_micros = SUM(ledger_entries.amount_micros)` for every account —
-- is what the integration suite checks, because a cached total that can drift from its source is
-- worse than a SUM: it is wrong quietly.
CREATE TABLE IF NOT EXISTS balances (
    account_id     UUID PRIMARY KEY REFERENCES accounts(account_id),
    balance_micros BIGINT NOT NULL DEFAULT 0,
    -- Not for display: for spotting a balance that stopped moving while entries kept arriving, which
    -- is what a broken write path looks like from the outside.
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    entry_count    BIGINT NOT NULL DEFAULT 0
);

-- Backfill for a database that already has entries (this migration lands after M2.3's).
--
-- `DO NOTHING`, emphatically **not** `DO UPDATE`. The first version overwrote every balance with a
-- snapshot computed at that instant, which is destructive rather than idempotent: migrations run on
-- every boot, so a rolling deploy would stomp live balances mid-traffic. The integration suite caught
-- it — several tests each call `migrate` while another is appending, and the balance came back
-- missing whatever had been written in between. The advisory lock serialises migrations against each
-- other; nothing serialises them against application traffic, so a migration has to be safe to run
-- *while* the system is working.
INSERT INTO balances (account_id, balance_micros, entry_count)
SELECT account_id, COALESCE(SUM(amount_micros), 0), COUNT(*)
FROM ledger_entries
GROUP BY account_id
ON CONFLICT (account_id) DO NOTHING;
