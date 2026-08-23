-- M11.10: the exact-response cache, shared by every replica (docs/11 §Caching).
--
-- UNLOGGED on purpose, and the first unlogged table in this schema. Two consequences an operator
-- must be told rather than discover: the table is TRUNCATED after an unclean Postgres shutdown,
-- and it is not replicated, so a standby cannot serve cache reads. Both are correct for a table
-- whose every row costs exactly one provider call to rebuild, and neither can fail a request — a
-- missing row is a miss, which is `ExactCache`'s whole contract.
--
-- tenant-scoping: the PRIMARY KEY, not a column beside it. docs/20 M20.3's cache-key audit exists
-- to make exactly this finding: a cache keyed on the digest alone lets one tenant's write
-- overwrite another tenant's row for the same prompt, after which the first tenant reads the
-- second's answer. Identical prompts across tenants are what a shared eval harness produces, so
-- the collision is likely rather than theoretical — and the leak looks like a cache working well.
-- `account_id` leads the key so a lookup and a per-account purge use the same index.
--
-- No FOREIGN KEY to `accounts`, unlike every other tenant table here, and the omission is a
-- decision. A cache row is disposable: it is unreachable once the account is gone (the account is
-- half the key), and it expires on its own within a TTL. The referential check would buy nothing
-- and would put an index probe on a write that runs in front of the caller's last stream frame.
CREATE UNLOGGED TABLE IF NOT EXISTS exact_cache (
    account_id     uuid        NOT NULL,
    -- sha256 of the normalized request (`panday_gateway::cache::CacheKey::of`).
    digest         text        NOT NULL,
    -- `panday_gateway::cache::CACHED_RESPONSE_SCHEMA_VERSION`. `StreamItem` has no `Unknown`
    -- fallback variant, so a row written by a binary carrying a variant this one lacks cannot be
    -- deserialized. A version this binary does not recognise reads as a MISS, never an error:
    -- during a rolling deploy both binaries share this table, and one unreadable row must not
    -- poison a hot key for a whole TTL.
    schema_version integer     NOT NULL,
    -- `Vec<StreamItem>`, in the order the provider produced them — including the `Usage` frame as
    -- the provider reported it. The read path rewrites usage as cache reads; storing the rewritten
    -- form would lose what the answer originally cost.
    items          jsonb       NOT NULL,
    written_at     timestamptz NOT NULL DEFAULT now(),
    -- The database's clock, not the writer's. `Instant` (what the in-memory cache uses) has no
    -- meaning across processes, and two replicas' wall clocks disagree by more than a short TTL.
    expires_at     timestamptz NOT NULL,
    PRIMARY KEY (account_id, digest)
);

-- The reaper's only access path. Not a partial index: the predicate moves with `now()`.
CREATE INDEX IF NOT EXISTS exact_cache_expires_at ON exact_cache (expires_at);
