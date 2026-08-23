//! Exact-response cache (M11.6, docs/11 §Caching).
//!
//! > "hash(normalized request) → response, PG unlogged table, TTL per route,
//! > only for `temperature=0` + no-tools requests (evals love this; agents
//! > rarely hit it)."
//!
//! Two things this deliberately is not.
//!
//! It is **not the cache that matters**. docs/11 says so in the next paragraph:
//! the provider prompt cache is the real one, and ADR-008 is where the money is.
//! This one exists because an eval suite replays identical requests hundreds of
//! times, and paying for that twice is silly.
//!
//! **Postgres lives elsewhere.** M11.10 built the PG unlogged table the spec names, but it is
//! `panday_platform::exact_cache::PgExactCache`, not a third type in this file: `panday-gateway`
//! has no `sqlx` dependency and must not grow one (docs/01, CLAUDE.md §6 — libraries take traits,
//! binaries do the wiring). What lives here is the trait, the two implementations that need no
//! database, and the parts with the bugs in them either way — eligibility, key normalization and
//! tenant scoping.
//!
//! [`conformance`] is the contract all three share. It was written against the two here *before*
//! the Postgres one existed, which is the ordering that made M25.11 cheap: a trait whose
//! implementations are tested separately is a trait with an unknown contract.

pub mod conformance;

use panday_types::id::AccountId;
use panday_types::model::{ChatRequest, StreamItem};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A cached response: the stream items, in order, as the provider produced them.
///
/// Items rather than text, so a cache hit is indistinguishable from a live call
/// to the caller — including the `Usage` frame. Serving a hit without usage would
/// make cached traffic invisible to the ledger; serving it *with* the original
/// usage would bill the caller twice for tokens spent once. The `Usage` is
/// therefore rewritten on the way out (see `Gateway::chat`).
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub items: Vec<StreamItem>,
}

/// Where exact-cache entries live.
///
/// Postgres implements this at **M11.10** (`panday_platform::exact_cache`). This line used to say
/// M3.5, which is ledger-rebuild-from-log (`docs/03-protocol.md`) and never owned the PG lane —
/// the same stale pointer `docs/11-gateway.md` had already corrected in prose while this copy was
/// missed.
///
/// **Async because a backend may be a database.** A synchronous `get` over Postgres has no legal
/// implementation on a tokio worker: `block_in_place` requires a multi-thread runtime and
/// `tests/overhead.rs` builds a current-thread one, while `Handle::block_on` panics inside a
/// runtime. This is the argument `UsageSink` already made — the sibling seams on this `Gateway`
/// (`UsageSink`, `RouteAudit`, `BudgetGate`) are async trait objects and `ExactCache` was the
/// holdout. `#[async_trait]` rather than a native `async fn` because the trait is only ever used
/// as `Arc<dyn ExactCache>`, and RPITIT is not dyn-compatible.
///
/// **Both methods are infallible on purpose.** A backend that is down, slow, or holding an
/// unreadable row has exactly one sound caller behaviour — treat it as a miss and carry on — so a
/// `Result` would hand `Gateway::chat` a decision it must never make differently, and the first
/// careless `?` would turn one bad row into a 500 for a hot key for a whole TTL. An implementation
/// logs and returns.
///
/// **Neither method may stall the request path.** The caller bounds both, so a slow backend costs
/// a miss and a dropped write rather than a hung stream. That bound lives at the call site rather
/// than in each implementation, so it holds for implementations nobody has written yet.
///
/// The contract, pinned by [`conformance::run`]:
///
/// 1. A `put` is readable by a later `get` on the same instance, item for item, in order, until
///    its TTL elapses.
/// 2. A key never written is a miss; an expired key is a miss.
/// 3. TTLs are per entry — one entry expiring does not disturb another.
/// 4. `account` is part of the key. No implementation may key on `digest` alone (docs/20 M20.3).
/// 5. A second `put` under one key replaces the first, and concurrent puts leave exactly one
///    readable value, which is one of the values written.
/// 6. Entries are immutable and there is no invalidation: the TTL is the only way one leaves.
///    Requests are `temperature=0` and tool-free ([`is_cacheable`]), so every value ever stored
///    under one key is interchangeable with every other.
#[async_trait::async_trait]
pub trait ExactCache: Send + Sync {
    async fn get(&self, key: &CacheKey) -> Option<CachedResponse>;
    async fn put(&self, key: CacheKey, response: CachedResponse, ttl: Duration);
}

/// Version of the encoding [`CachedResponse`] is persisted in.
///
/// `StreamItem` has no golden fixture and no `proto/` export, so nothing today makes a change to
/// its shape a reviewed protocol change — and M11.10 turned it into a *durable* format. Unlike
/// `Event` it has no `Unknown` fallback variant, so a row written by a binary carrying a variant
/// this one lacks cannot be deserialized.
///
/// **Bump this on any `StreamItem` shape change.** A row whose version differs is read as a miss,
/// never an error: during a rolling deploy both binaries share one table, and one unreadable row
/// must not poison a hot key for a whole TTL. `the_persisted_encoding_is_pinned` below is the
/// guard that makes the bump impossible to forget.
pub const CACHED_RESPONSE_SCHEMA_VERSION: i32 = 1;

/// The cache key. **Tenant-scoped by construction**: `account` is part of the
/// key, not an afterthought.
///
/// docs/20 M20.3 calls for a "cache key audit", and this is the finding it would
/// make: a key that omitted the account would let one tenant's prompt serve
/// another tenant's response. Identical prompts across tenants are exactly what
/// a shared eval harness produces, so the collision is likely rather than
/// theoretical, and the leak would look like a cache working well.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub account: AccountId,
    /// sha256 of the normalized request.
    pub digest: String,
}

impl CacheKey {
    /// Hash the parts of a request that change the response, and nothing else.
    ///
    /// Excluded on purpose: `request_id`, `session`, `turn`, `task` (ids and
    /// attribution — including them would make every key unique and the cache a
    /// memory leak with a 0% hit rate), and `stream` (the same completion,
    /// delivered differently).
    ///
    /// Included on purpose: `model`, every message, sampling, and the cache
    /// hints — `extended_ttl` and breakpoints change what the provider is asked
    /// for, and a request that differs in them is a different request.
    pub fn of(req: &ChatRequest) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(req.model.0.as_bytes());
        h.update([0]);
        // serde_json over the typed messages: a hand-rolled walk would silently
        // stop covering a field the moment one is added, and a cache that ignores
        // a new field serves stale answers for it.
        h.update(serde_json::to_vec(&req.messages).unwrap_or_default());
        h.update([0]);
        h.update(serde_json::to_vec(&req.sampling).unwrap_or_default());
        h.update([0]);
        h.update(serde_json::to_vec(&req.cache).unwrap_or_default());
        Self {
            account: req.metadata.account,
            digest: panday_types::hex(h.finalize()),
        }
    }
}

/// Whether this request may be served from, or stored in, the exact cache.
///
/// docs/11: "only for `temperature=0` + no-tools requests". Both halves matter:
///
/// - A request with tools is part of an agent loop whose next step depends on
///   real execution; replaying a cached tool call would have the harness act on
///   a decision made about a different workspace.
/// - Absent temperature is *not* zero. Providers default to ~1, so treating
///   "unset" as deterministic would cache one sample of a distribution and serve
///   it forever — the failure would look like a model that stopped thinking.
pub fn is_cacheable(req: &ChatRequest) -> bool {
    req.tools.is_empty() && req.sampling.temperature == Some(0.0)
}

/// In-memory exact cache with a TTL and a bounded size.
///
/// Bounded because a gateway is long-lived and an unbounded map keyed by request
/// hash is a memory leak with extra steps. Eviction is oldest-inserted-first:
/// LRU would be better and needs an access-ordered structure, which is not worth
/// it for a cache whose stated audience is eval replay.
pub struct MemoryExactCache {
    entries: Mutex<HashMap<CacheKey, Entry>>,
    order: Mutex<std::collections::VecDeque<CacheKey>>,
    capacity: usize,
}

struct Entry {
    response: CachedResponse,
    expires: Instant,
}

impl MemoryExactCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(std::collections::VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryExactCache {
    fn default() -> Self {
        Self::new(1024)
    }
}

// These futures are `Send` despite the `std::sync::MutexGuard`s only because no `.await` sits
// between a lock and the return. Do not add one: a guard held across an await here surfaces as a
// trait-bound error in `capture_usage`, a long way from the edit that caused it.
#[async_trait::async_trait]
impl ExactCache for MemoryExactCache {
    async fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get(key)?;
        if entry.expires <= Instant::now() {
            // Expiring on read rather than on a timer: a gateway with no traffic
            // should not be waking up to tidy a cache nobody is reading.
            entries.remove(key);
            return None;
        }
        Some(entry.response.clone())
    }

    async fn put(&self, key: CacheKey, response: CachedResponse, ttl: Duration) {
        let mut entries = self.entries.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        // `is_new` gates the push as well as the eviction. Pushing unconditionally — which this
        // did — grows `order` without bound when one hot key is re-put, and each stale duplicate
        // later pops and evicts a *live* entry. The pop loop below tolerates a residual stale
        // entry (expire-on-read can remove from `entries` without touching `order`), which is why
        // it skips misses rather than stopping at the first one.
        let is_new = !entries.contains_key(&key);
        if entries.len() >= self.capacity && is_new {
            while let Some(oldest) = order.pop_front() {
                if entries.remove(&oldest).is_some() {
                    break;
                }
            }
        }
        if is_new {
            order.push_back(key.clone());
        }
        entries.insert(
            key,
            Entry {
                response,
                expires: Instant::now() + ttl,
            },
        );
    }
}

/// Discards everything. The default, because a cache is an opt-in behaviour
/// change: docs/11 gives it a per-route TTL, and a gateway with no TTL configured
/// has not been told to cache anything.
pub struct NoCache;

#[async_trait::async_trait]
impl ExactCache for NoCache {
    async fn get(&self, _key: &CacheKey) -> Option<CachedResponse> {
        None
    }
    async fn put(&self, _key: CacheKey, _response: CachedResponse, _ttl: Duration) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> CacheKey {
        CacheKey {
            account: AccountId(uuid::Uuid::from_u128(1)),
            digest: format!("{n:064x}"),
        }
    }

    #[tokio::test]
    async fn re_putting_one_key_does_not_grow_the_eviction_queue() {
        // `put` used to push to `order` unconditionally while the eviction guard checked
        // `!contains_key`, so re-putting one key appended a duplicate every time and never
        // removed one. `order` grew without bound — a leak in the one structure whose stated
        // justification is being bounded ("an unbounded map keyed by request hash is a memory leak
        // with extra steps"), and invisible from outside because `len()` reports `entries`.
        //
        // An eval suite replaying identical requests is exactly the traffic this cache exists for,
        // so the leak is on the main path rather than an edge case.
        let cache = MemoryExactCache::new(2);
        let response = CachedResponse { items: vec![] };
        for _ in 0..64 {
            cache
                .put(key(1), response.clone(), Duration::from_secs(60))
                .await;
        }
        assert_eq!(
            cache.order.lock().unwrap().len(),
            1,
            "the eviction queue grew once per write instead of once per distinct key"
        );
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn a_stale_queue_entry_does_not_stop_eviction() {
        // Expire-on-read removes from `entries` without touching `order`, so a stale name can
        // still reach the front of the queue. The pop loop must skip it and keep going, or the
        // cache stops evicting and grows past its capacity.
        let cache = MemoryExactCache::new(2);
        let response = CachedResponse { items: vec![] };
        cache
            .put(key(1), response.clone(), Duration::from_millis(0))
            .await;
        assert!(cache.get(&key(1)).await.is_none(), "expired on read");

        for n in 2..=6 {
            cache
                .put(key(n), response.clone(), Duration::from_secs(60))
                .await;
        }
        assert!(cache.len() <= 2, "capacity was {}", cache.len());
    }
}
