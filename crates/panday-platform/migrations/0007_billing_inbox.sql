-- M17.4: the webhook inbox, and the meter export cursor.
--
-- docs/17: "webhooks (`checkout.completed`, `invoice.paid`, `sub.updated`) drive plan state via an
-- **idempotent, replayable webhook inbox table** (never trust webhook delivery; poll-reconcile
-- nightly)".
--
-- The inbox is the whole design. A webhook handler that applies an effect and returns 200 has three
-- ways to be wrong — the effect applied twice, the 200 lost, the event arriving out of order — and
-- all three go away if receiving and applying are separate steps over a durable row.
CREATE TABLE IF NOT EXISTS billing_events (
    -- Stripe's own event id. The primary key, so a redelivery is a no-op at the database level
    -- rather than at whatever code path happens to run.
    event_id     text PRIMARY KEY,
    kind         text NOT NULL,
    -- The event as received, entire. Storing our interpretation instead would make a replay
    -- reproduce the bug rather than fix it.
    payload      jsonb NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    -- NULL until applied. A row that is received and not applied is the queue.
    applied_at   timestamptz,
    -- Set when applying failed. Kept with the row rather than in a log: the thing that failed and
    -- the reason belong together when somebody replays it.
    error        text,
    attempts     integer NOT NULL DEFAULT 0,
    -- Which account it touched, once known. NULL for events we could not attribute — which is a
    -- state worth being able to query for.
    account_id   uuid REFERENCES accounts(account_id)
);

-- "What is stuck" — the query the nightly reconcile runs, and the one an operator runs at 3am.
CREATE INDEX IF NOT EXISTS billing_events_pending
    ON billing_events (received_at)
    WHERE applied_at IS NULL;

-- M17.5: how far the meter export has got.
--
-- One row per (account, hour, meter): the export is idempotent because re-exporting an hour that
-- was already sent is a primary-key collision, not a duplicate charge. Stripe aggregates
-- asynchronously and cannot deduplicate for us (ADR-009), so the cursor has to be ours.
CREATE TABLE IF NOT EXISTS meter_exports (
    account_id   uuid NOT NULL REFERENCES accounts(account_id),
    -- The hour this covers, truncated. Hourly because docs/17 says hourly, and because an hour is
    -- short enough to retry cheaply and long enough that a busy account is not a write storm.
    hour         timestamptz NOT NULL,
    meter        text NOT NULL,
    -- What we told Stripe. Kept so the invoice can be checked against it line by line (M17.5).
    value        bigint NOT NULL CHECK (value >= 0),
    exported_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, hour, meter)
);
