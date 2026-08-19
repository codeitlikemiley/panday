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
//! It is **not Postgres**. The spec names a PG unlogged table, and PG is M3.5 —
//! so this is the trait plus an in-memory implementation, and the binaries wire
//! whichever they have (docs/01: libraries take traits). The eligibility rules,
//! the key normalization and the tenant scoping are the parts with the bugs in
//! them, and they live here either way.

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

/// Where exact-cache entries live. Postgres implements this at M3.5.
pub trait ExactCache: Send + Sync {
    fn get(&self, key: &CacheKey) -> Option<CachedResponse>;
    fn put(&self, key: CacheKey, response: CachedResponse, ttl: Duration);
}

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
            digest: format!("{:x}", h.finalize()),
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

impl ExactCache for MemoryExactCache {
    fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
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

    fn put(&self, key: CacheKey, response: CachedResponse, ttl: Duration) {
        let mut entries = self.entries.lock().unwrap();
        let mut order = self.order.lock().unwrap();
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            while let Some(oldest) = order.pop_front() {
                if entries.remove(&oldest).is_some() {
                    break;
                }
            }
        }
        order.push_back(key.clone());
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

impl ExactCache for NoCache {
    fn get(&self, _key: &CacheKey) -> Option<CachedResponse> {
        None
    }
    fn put(&self, _key: CacheKey, _response: CachedResponse, _ttl: Duration) {}
}
