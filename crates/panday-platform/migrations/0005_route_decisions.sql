-- M12.2: where the router sent each request, and what actually served it.
--
-- Content-free by construction (docs/20 T5): model ids, a rule name, a pool, counts. No prompt and
-- no completion, which is what makes the table safe to keep long enough to answer the questions it
-- exists for — which rule sends traffic where, how often that chain fails over, and which pool the
-- money is going to.
--
-- One row per request, not per attempt: `attempts` and `chosen` carry the failover story, and a row
-- per leg would multiply the largest table in the system by the failure rate of the worst provider.
CREATE TABLE IF NOT EXISTS route_decisions (
    request_id      uuid PRIMARY KEY,
    account_id      uuid NOT NULL REFERENCES accounts(account_id),
    -- What the caller asked for: `auto`, or the model they pinned.
    requested       text NOT NULL,
    task            text NOT NULL,
    matched_rule    text NOT NULL,
    pool            text NOT NULL,
    -- The resolved chain, in failover order.
    chain           jsonb NOT NULL,
    -- NULL means every leg failed. These are the rows worth alerting on.
    chosen          text,
    attempts        integer NOT NULL CHECK (attempts >= 0),
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- "What did this account route to last week" — the support question, and the one an account-scoped
-- query has to answer without a sequential scan of every tenant's traffic.
CREATE INDEX IF NOT EXISTS route_decisions_account_time
    ON route_decisions (account_id, created_at DESC);

-- "Which rules are firing, and which of them fail over" — the operator question (docs/21).
CREATE INDEX IF NOT EXISTS route_decisions_rule_time
    ON route_decisions (matched_rule, created_at DESC);
