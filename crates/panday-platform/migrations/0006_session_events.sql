-- M18.6: offline sessions, synced.
--
-- The event log as it arrives from a laptop that was working offline (docs/18 §Sync). Append-only
-- and single-writer per session (ADR-002), which is what makes the merge trivial: there is no
-- conflict to resolve, only events this account has not seen yet.
--
-- `(session_id, seq)` is the primary key, so re-pushing a log is a no-op rather than a duplicate.
-- A laptop that reconnects twice — or one that syncs, crashes, and syncs again — must not double
-- anything, and the cheapest way to guarantee that is to make the second write impossible.
CREATE TABLE IF NOT EXISTS session_events (
    session_id  uuid NOT NULL,
    -- Gapless per session; the ordering primitive the whole protocol rests on (docs/03).
    seq         bigint NOT NULL CHECK (seq >= 0),
    account_id  uuid NOT NULL REFERENCES accounts(account_id),
    at          timestamptz NOT NULL,
    -- The event kind, denormalised so a query can filter without parsing every payload. The
    -- payload is the whole envelope, because an event we do not understand today must still be
    -- stored and re-served unchanged (docs/03 §unknown kinds).
    kind        text NOT NULL,
    envelope    jsonb NOT NULL,
    synced_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, seq)
);

-- "What has this account been doing" — the support and dashboard query, account-scoped so one
-- tenant's traffic never scans another's.
CREATE INDEX IF NOT EXISTS session_events_account_time
    ON session_events (account_id, synced_at DESC);
