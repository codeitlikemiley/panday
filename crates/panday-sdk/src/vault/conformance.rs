//! One conformance suite, every `CredentialStore` (docs/25 M25.11).
//!
//! The matrix is [`run`]. An implementation calls it and inherits every invariant
//! the gateway and CLI depend on; adding Postgres is one more call site, not
//! another copy of these assertions.
//!
//! It is written *before* the third store rather than after, because the two that
//! already exist had never been held to the same list. `MemoryStore` was the only
//! one whose grant behaviour was tested, and `SqliteStore` the only one whose
//! duplicate-id rejection was — so each was trusted for something the other had
//! never been asked to do. A trait with two implementations and two disjoint test
//! sets is a trait with an unknown contract.
//!
//! Public rather than `#[cfg(test)]` because implementations live in other
//! modules and, for Postgres, behind an integration-lane feature.

use super::{CredentialId, CredentialMeta, CredentialStore, Kind, State, VaultError};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Builds a fresh, empty store. Called several times; each call must produce a
/// store that shares nothing with the previous one.
pub type Factory =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn CredentialStore>> + Send>> + Send + Sync>;

const SECRET_A: &str = "sk-test-aaaa";
const SECRET_B: &str = "sk-test-bbbb";

fn meta(provider: &str, label: &str) -> CredentialMeta {
    CredentialMeta {
        id: CredentialId::new(),
        provider: provider.into(),
        kind: Kind::ApiKey,
        label: label.into(),
        last4: String::new(),
        state: State::Active,
        ceiling: None,
        window_secs: None,
    }
}

/// Every invariant a `CredentialStore` owes its callers.
///
/// `name` is only used in assertion messages, so a failure says which store broke.
pub async fn run(name: &str, make: Factory) {
    round_trips_and_keeps_the_secret_out_of_list(name, &make).await;
    a_duplicate_id_is_rejected(name, &make).await;
    an_empty_secret_is_rejected(name, &make).await;
    rows_decrypt_independently(name, &make).await;
    a_missing_row_is_not_found(name, &make).await;
    revoke_wipes_the_ciphertext_and_keeps_the_row(name, &make).await;
    revoke_is_the_only_way_to_reach_revoked(name, &make).await;
    a_grant_round_trips_and_clears(name, &make).await;
    a_grant_on_a_missing_row_is_not_found(name, &make).await;
}

async fn round_trips_and_keeps_the_secret_out_of_list(name: &str, make: &Factory) {
    let store = make().await;
    let m = meta("xai", "one");
    let id = m.id;
    store.put(m, SECRET_A).await.expect("put");

    assert_eq!(
        store.get_secret(&id).await.expect("get").expose(),
        SECRET_A,
        "{name}: the secret must survive a round trip"
    );

    let rows = store.list().await.expect("list");
    assert_eq!(rows.len(), 1, "{name}: one row in, one row out");
    assert_eq!(
        rows[0].last4, "aaaa",
        "{name}: last4 is stored in the clear"
    );
    // `list` is what the console renders. A store that leaked the secret into a
    // metadata row would put it on an operator's screen.
    assert!(
        !format!("{:?}", rows[0]).contains(SECRET_A),
        "{name}: list must never carry the secret"
    );
}

async fn a_duplicate_id_is_rejected(name: &str, make: &Factory) {
    let store = make().await;
    let m = meta("xai", "one");
    let same_id = m.id;
    store.put(m, SECRET_A).await.expect("first put");

    let mut dup = meta("anthropic", "two");
    dup.id = same_id;
    // Silently overwriting would replace one credential's ciphertext with
    // another's under an id the caller still believes in.
    assert!(
        matches!(
            store.put(dup, SECRET_B).await,
            Err(VaultError::AlreadyExists)
        ),
        "{name}: a duplicate id must be refused, not overwritten"
    );
}

async fn an_empty_secret_is_rejected(name: &str, make: &Factory) {
    let store = make().await;
    assert!(
        matches!(
            store.put(meta("xai", "empty"), "").await,
            Err(VaultError::EmptySecret)
        ),
        "{name}: an empty secret is a bug upstream, not a credential"
    );
}

async fn rows_decrypt_independently(name: &str, make: &Factory) {
    let store = make().await;
    let (a, b) = (meta("xai", "a"), meta("anthropic", "b"));
    let (ida, idb) = (a.id, b.id);
    store.put(a, SECRET_A).await.expect("put a");
    store.put(b, SECRET_B).await.expect("put b");

    // The AAD binds id/provider/kind, so a store that mixed rows up would fail
    // to decrypt rather than return the wrong secret — but assert the values.
    assert_eq!(
        store.get_secret(&ida).await.expect("a").expose(),
        SECRET_A,
        "{name}: one row's secret must not bleed into another's"
    );
    assert_eq!(
        store.get_secret(&idb).await.expect("b").expose(),
        SECRET_B,
        "{name}: one row's secret must not bleed into another's"
    );
}

async fn a_missing_row_is_not_found(name: &str, make: &Factory) {
    let store = make().await;
    assert!(
        matches!(
            store.get_secret(&CredentialId::new()).await,
            Err(VaultError::NotFound)
        ),
        "{name}: an unknown id is NotFound, not an empty success"
    );
}

async fn revoke_wipes_the_ciphertext_and_keeps_the_row(name: &str, make: &Factory) {
    let store = make().await;
    let m = meta("xai", "doomed");
    let id = m.id;
    store.put(m, SECRET_A).await.expect("put");
    store.revoke(&id).await.expect("revoke");

    let rows = store.list().await.expect("list");
    assert_eq!(rows.len(), 1, "{name}: a revoked row is kept, not deleted");
    assert_eq!(rows[0].state, State::Revoked);
    assert_eq!(
        rows[0].last4, "aaaa",
        "{name}: usage history still has to name it"
    );
    assert!(
        matches!(store.get_secret(&id).await, Err(VaultError::Revoked)),
        "{name}: a revoked secret is gone, and says so distinctly from NotFound"
    );
}

async fn revoke_is_the_only_way_to_reach_revoked(name: &str, make: &Factory) {
    let store = make().await;
    let m = meta("xai", "states");
    let id = m.id;
    store.put(m, SECRET_A).await.expect("put");

    // `set_state(Revoked)` would move the row to a state whose whole meaning is
    // "the ciphertext is wiped" without wiping it.
    assert!(
        matches!(
            store.set_state(&id, State::Revoked).await,
            Err(VaultError::InvalidMeta)
        ),
        "{name}: set_state must not be a back door to Revoked"
    );

    store
        .set_state(&id, State::Exhausted)
        .await
        .expect("a normal transition");
    assert_eq!(store.list().await.expect("list")[0].state, State::Exhausted);

    store.revoke(&id).await.expect("revoke");
    assert!(
        matches!(
            store.set_state(&id, State::Active).await,
            Err(VaultError::Revoked)
        ),
        "{name}: revocation is terminal"
    );
}

async fn a_grant_round_trips_and_clears(name: &str, make: &Factory) {
    let store = make().await;
    let m = meta("xai", "granted");
    let id = m.id;
    store.put(m, SECRET_A).await.expect("put");
    assert_eq!(store.list().await.expect("list")[0].ceiling, None);

    store
        .set_grant(&id, Some(500), Some(18_000))
        .await
        .expect("set");
    let row = store.list().await.expect("list").remove(0);
    assert_eq!(row.ceiling, Some(500), "{name}: ceiling persists");
    assert_eq!(row.window_secs, Some(18_000), "{name}: window persists");

    // Editing bookkeeping must not disturb the one value here worth protecting.
    assert_eq!(
        store.get_secret(&id).await.expect("get").expose(),
        SECRET_A,
        "{name}: set_grant must not touch the ciphertext"
    );

    store.set_grant(&id, None, None).await.expect("clear");
    assert_eq!(
        store.list().await.expect("list")[0].ceiling,
        None,
        "{name}: clearing a grant is expressible"
    );
}

async fn a_grant_on_a_missing_row_is_not_found(name: &str, make: &Factory) {
    let store = make().await;
    assert!(
        matches!(
            store.set_grant(&CredentialId::new(), Some(1), None).await,
            Err(VaultError::NotFound)
        ),
        "{name}: setting a grant on nothing is an error, not a silent no-op"
    );
}
