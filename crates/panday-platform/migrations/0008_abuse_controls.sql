-- M17.7 / M20.4: the kill switch, and what it records.
--
-- Suspension is a timestamp and a reason on the account, not a delete and not a flag. A deleted
-- account cannot be investigated and cannot be reinstated; a bare boolean cannot answer "who did
-- this, and why" three weeks later when the customer writes in.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS suspended_at timestamptz;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS suspended_reason text;

-- Every administrative action, append-only.
--
-- docs/20 asks for SOC2-shaped controls "without the audit", and the cheapest honest version of
-- "access logs + change review" is a table nothing updates. An admin action that leaves no trace is
-- indistinguishable from an intrusion.
CREATE TABLE IF NOT EXISTS admin_actions (
    action_id   uuid PRIMARY KEY,
    -- The account acted upon. NULL for actions that are not about one account.
    account_id  uuid REFERENCES accounts(account_id),
    -- suspend | unsuspend | revoke_key | grant | refund
    action      text NOT NULL,
    -- Who did it: the admin key's id. Not a name — names change, ids do not.
    actor       text NOT NULL,
    reason      text,
    at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS admin_actions_account_time
    ON admin_actions (account_id, at DESC);
