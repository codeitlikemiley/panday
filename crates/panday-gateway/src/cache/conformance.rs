//! One conformance suite, every `ExactCache` (docs/11 §Caching, M11.10).
//!
//! Written against `MemoryExactCache` *before* `PgExactCache` existed. That ordering is the whole
//! technique: `CredentialStore` (M25.11) reached its second implementation with two disjoint test
//! sets and therefore an unknown contract, and reconstructing it afterwards was the expensive
//! part. `ExactCache` had one implementation and was about to have two, which is the moment to
//! write the contract down rather than the moment after.
//!
//! Public rather than `#[cfg(test)]`, because the Postgres implementation lives in another crate
//! and behind the integration lane.
//!
//! **What is deliberately absent.** Capacity and eviction order are `MemoryExactCache` concepts,
//! not trait ones — a PG unlogged table is bounded by disk and a reaper, and `len()` is an
//! inherent method, not part of the trait. Asserting them here would fail the Postgres
//! implementation on a promise it never made.

use super::{CacheKey, CachedResponse, ExactCache};
use panday_types::id::AccountId;
use panday_types::model::{StopReason, StreamItem, Usage};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Builds a fresh, empty cache sharing nothing with the previous one, and able to hold at least
/// eight live entries.
///
/// The floor is not a trait requirement — it exists so the multi-key assertions below are not
/// accidentally testing `MemoryExactCache`'s eviction.
pub type Factory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn ExactCache>> + Send>> + Send + Sync>;

const LONG: Duration = Duration::from_secs(60);

fn key(account: u128, digest: &str) -> CacheKey {
    CacheKey {
        account: AccountId(uuid::Uuid::from_u128(account)),
        digest: digest.into(),
    }
}

/// A response with something in it worth getting wrong — non-ASCII text, an optional field that is
/// skipped when absent, and a `Usage` frame whose counters must survive.
fn response(text: &str) -> CachedResponse {
    CachedResponse {
        items: vec![
            StreamItem::Delta { text: text.into() },
            // A `Usage` frame, because a hit must be indistinguishable from a live call to the
            // caller — a store that dropped it would make cached traffic invisible to the ledger.
            StreamItem::Usage {
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 22,
                    ..Usage::default()
                },
            },
            StreamItem::Done {
                reason: StopReason::EndTurn,
            },
        ],
    }
}

/// Every invariant an `ExactCache` owes its callers.
///
/// `name` appears in assertion messages so a failure says which implementation broke.
pub async fn run(name: &str, make: Factory) {
    a_response_round_trips_item_for_item(name, &make).await;
    an_empty_response_is_a_stored_value_not_a_miss(name, &make).await;
    a_key_never_written_is_a_miss(name, &make).await;
    an_expired_entry_is_a_miss(name, &make).await;
    a_live_entry_survives_an_unrelated_expiry(name, &make).await;
    two_accounts_with_one_digest_each_read_their_own_body(name, &make).await;
    a_second_put_replaces_the_first(name, &make).await;
    an_unrelated_put_does_not_disturb_a_live_key(name, &make).await;
    concurrent_puts_of_one_key_leave_exactly_one_readable_value(name, &make).await;
}

async fn a_response_round_trips_item_for_item(name: &str, make: &Factory) {
    let cache = make().await;
    let k = key(1, "round-trip");
    // Non-ASCII and a multi-byte grapheme: a store that round-trips through a lossy encoding, or
    // truncates on a byte boundary, serves a subtly different answer than the provider gave —
    // forever, to everyone who asks the same question.
    let stored = response("héllo — 世界 🔨");
    cache.put(k.clone(), stored.clone(), LONG).await;

    let got = cache.get(&k).await.expect("a value just written");
    assert_eq!(
        format!("{:?}", got.items),
        format!("{:?}", stored.items),
        "{name}: a cached response must come back item for item, in order"
    );
}

async fn an_empty_response_is_a_stored_value_not_a_miss(name: &str, make: &Factory) {
    let cache = make().await;
    let k = key(1, "empty");
    cache
        .put(k.clone(), CachedResponse { items: vec![] }, LONG)
        .await;
    // "Stored, and empty" is not "absent". An implementation that reads `[]` as nothing turns a
    // legitimate cached response into a provider call on every request, forever.
    assert!(
        cache.get(&k).await.is_some(),
        "{name}: an empty item list is a stored value, not a miss"
    );
}

async fn a_key_never_written_is_a_miss(name: &str, make: &Factory) {
    let cache = make().await;
    // The trait cannot report an error, so a store whose empty-result handling is wrong (a
    // `fetch_one` where `fetch_optional` was meant) has nowhere to put the failure.
    assert!(
        cache.get(&key(1, "never-written")).await.is_none(),
        "{name}: an unknown key is a miss"
    );
}

async fn an_expired_entry_is_a_miss(name: &str, make: &Factory) {
    let cache = make().await;
    let k = key(1, "expired");
    cache
        .put(k.clone(), response("stale"), Duration::ZERO)
        .await;
    assert!(
        cache.get(&k).await.is_none(),
        "{name}: an expired entry must not be served — a lookup that ignores the deadline serves \
         stale answers with no bound"
    );
}

async fn a_live_entry_survives_an_unrelated_expiry(name: &str, make: &Factory) {
    let cache = make().await;
    let (live, dead) = (key(1, "live"), key(1, "dead"));
    cache.put(live.clone(), response("fresh"), LONG).await;
    cache
        .put(dead.clone(), response("stale"), Duration::ZERO)
        .await;

    // TTLs are per entry. A store with one table-wide deadline, or a reaper that deletes by table
    // rather than by row, takes the live one with it.
    assert!(cache.get(&dead).await.is_none(), "{name}: the expired one");
    assert!(
        cache.get(&live).await.is_some(),
        "{name}: one entry expiring must not disturb another"
    );
}

async fn two_accounts_with_one_digest_each_read_their_own_body(name: &str, make: &Factory) {
    let cache = make().await;
    // The one that matters. docs/20 M20.3's finding is that the key is tenant-scoped by
    // construction — but the only test that existed asserted a second account *misses*, and an
    // implementation keyed on the digest alone passes that: tenant B's write overwrites tenant A's
    // row, B misses as asserted, and A then reads B's answer on its next request.
    //
    // Identical prompts across tenants are exactly what a shared eval harness produces, so the
    // collision is likely rather than theoretical, and the leak looks like a cache working well.
    let (a, b) = (key(1, "same-digest"), key(2, "same-digest"));
    cache
        .put(a.clone(), response("account one's answer"), LONG)
        .await;
    cache
        .put(b.clone(), response("account two's answer"), LONG)
        .await;

    let got_a = cache.get(&a).await.expect("account one");
    let got_b = cache.get(&b).await.expect("account two");
    assert_eq!(
        format!("{:?}", got_a.items),
        format!("{:?}", response("account one's answer").items),
        "{name}: one account read another's cached response — the digest is not the key, \
         (account, digest) is"
    );
    assert_eq!(
        format!("{:?}", got_b.items),
        format!("{:?}", response("account two's answer").items),
        "{name}: the second account's own entry was clobbered"
    );
}

async fn a_second_put_replaces_the_first(name: &str, make: &Factory) {
    let cache = make().await;
    let k = key(1, "overwritten");
    cache.put(k.clone(), response("first"), LONG).await;
    cache.put(k.clone(), response("second"), LONG).await;

    // Last write wins. A store with a bare INSERT and no conflict clause either errors — silently,
    // since the trait cannot report it, so the entry stops refreshing and the key goes cold at
    // TTL — or duplicates the row.
    assert_eq!(
        format!("{:?}", cache.get(&k).await.expect("present").items),
        format!("{:?}", response("second").items),
        "{name}: a second put under one key must replace the first"
    );
}

async fn an_unrelated_put_does_not_disturb_a_live_key(name: &str, make: &Factory) {
    let cache = make().await;
    let (a, b) = (key(1, "kept"), key(1, "other"));
    cache.put(a.clone(), response("kept"), LONG).await;
    cache.put(b.clone(), response("other"), LONG).await;
    // Catches a store keyed on something coarser than the whole `CacheKey`, and catches the
    // unbounded-`order` bug that used to let a stale queue entry evict a live one.
    assert!(
        cache.get(&a).await.is_some(),
        "{name}: writing one key evicted an unrelated live key"
    );
}

async fn concurrent_puts_of_one_key_leave_exactly_one_readable_value(name: &str, make: &Factory) {
    let cache = make().await;
    let k = key(1, "raced");
    // Two simultaneous misses on one key both call the provider and both write: there is no
    // single-flight anywhere in the gateway, and a cache shared across replicas makes that window
    // visible. This decides the behaviour explicitly — a torn or interleaved value is a bug,
    // duplicate provider work is not — rather than letting whatever happens become the contract.
    let writes = (0..8).map(|i| {
        let cache = cache.clone();
        let k = k.clone();
        async move { cache.put(k, response(&format!("writer {i}")), LONG).await }
    });
    futures_util::future::join_all(writes).await;

    let got = cache.get(&k).await.expect("one of the writers won");
    let readable = format!("{:?}", got.items);
    assert!(
        (0..8).any(|i| readable == format!("{:?}", response(&format!("writer {i}")).items)),
        "{name}: concurrent puts left a value nobody wrote — the row is torn: {readable}"
    );
}
