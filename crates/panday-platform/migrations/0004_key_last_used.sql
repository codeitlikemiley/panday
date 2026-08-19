-- M17.3: last-used tracking (docs/17 §API keys for machines).
--
-- Separate from `revoked_at` because they answer different questions: revocation is a decision, and
-- last-used is evidence. The pair is what makes a key rotation reviewable — "this key has not been
-- used in 90 days" is the argument for revoking it, and "it was used an hour ago" is the argument
-- against.
--
-- Nullable, because a key that has never been used is a different state from one used at the epoch.
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ;
