-- M25.11: upstream credential ciphertext, same envelope as the laptop vault.
--
-- The hosted twin of `~/.panday/credentials.sqlite`. Identical envelope: XChaCha20-Poly1305,
-- AAD = `id || provider || kind`, nonce and ciphertext as bytes, no plaintext column and no
-- column that could become one. The KEK never reaches this database — on a hosted deployment it
-- comes from `PANDAY_VAULT_KEY`, because `~/.panday/master.key` and the macOS Keychain
-- (M25.12) both exist only on a laptop. A lost KEK is an unreadable vault, permanently.
--
-- tenant-scoping: DELIBERATELY NOT SCOPED. Every other table here carries
-- `account_id NOT NULL REFERENCES accounts(account_id)`; this one does not, and the omission is
-- the decision rather than an oversight. docs/25 opens by saying this is **not tenant BYOK** —
-- these are the *operator's* upstream credentials, the ones the gateway spends on everyone's
-- behalf, so there is no tenant they could belong to. Scoping them would invent an ownership
-- that does not exist and imply customers may supply their own, which is the thing docs/25
-- §Non-goals rules out first. If tenant BYOK ever ships it is a different table with a
-- different threat model, not an `account_id` bolted onto this one.
CREATE TABLE IF NOT EXISTS credentials (
    -- UUIDv4, not v7: a secret id must not encode its mint time (docs/25 §Domain).
    id          uuid PRIMARY KEY,
    provider    text NOT NULL,
    -- `api_key` | `oauth`. Part of the AAD, so changing it invalidates the row's ciphertext.
    kind        text NOT NULL,
    -- Operator's label. Never a secret; rendered on the console.
    label       text NOT NULL,
    -- Last four characters, in the clear by design: it is how a human tells two keys apart,
    -- and it is what a revoked row keeps so usage history can still name it.
    last4       text NOT NULL,
    -- `active` | `exhausted` | `invalid` | `revoked`.
    state       text NOT NULL,
    nonce       bytea NOT NULL,
    -- Wiped on revoke. The row survives; the secret does not.
    ciphertext  bytea NOT NULL,
    -- Operator-declared grant (M25.6). Calls per window, both null until declared —
    -- null is "nobody has measured this", which is not the same as zero.
    ceiling     bigint,
    window_secs bigint,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Boot reads every non-revoked row for a provider to seed its pool, and the console lists by
-- label. Neither is hot, but both are the only two access patterns there are.
CREATE INDEX IF NOT EXISTS credentials_provider_state_idx ON credentials (provider, state);
