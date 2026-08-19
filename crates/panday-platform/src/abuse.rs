//! Abuse guardrails (docs/17 §fraud posture, docs/20 M20.4, M17.7).
//!
//! > "trial abuse is a documented plague — free tier gets no T3, no frontier pool, hard concurrency
//! > caps, and disposable-email/device heuristics from day one."
//!
//! Three mechanisms, in increasing order of how much certainty they need:
//!
//! - **Velocity checks** are advisory. Thirty accounts from one signup source in an hour is a
//!   signal, not a verdict, and the response is to slow down or to flag — never to delete. A
//!   heuristic wired directly to an irreversible action will eventually be wrong about a real
//!   customer on their busiest day.
//! - **Disposable-email detection** is advisory too, and deliberately a *list* rather than a
//!   cleverness. A regex that guesses at throwaway domains catches a university and misses
//!   `mailinator`; a list is auditable, and a customer who writes in can be told exactly why.
//! - **The kill switch** is certain, immediate, and reversible: a timestamp and a reason on the
//!   account. Not a delete — a deleted account cannot be investigated and cannot be reinstated —
//!   and every use is written to an append-only audit table, because an admin action that leaves no
//!   trace is indistinguishable from an intrusion.

use crate::pg::PgError;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AbuseError {
    #[error(transparent)]
    Db(#[from] PgError),
    #[error("no such account")]
    NoSuchAccount,
}

/// Domains whose addresses are disposable by design.
///
/// A short, checked-in list rather than a live service: an abuse control that depends on a third
/// party is an abuse control that fails open when that party has an outage, and a list in the repo
/// is one a reviewer can argue with. It is meant to be extended by whoever is watching the signups,
/// which is why it is a plain constant and not a clever matcher.
pub const DISPOSABLE_DOMAINS: &[&str] = &[
    "mailinator.com",
    "guerrillamail.com",
    "10minutemail.com",
    "temp-mail.org",
    "throwawaymail.com",
    "yopmail.com",
    "trashmail.com",
    "sharklasers.com",
    "getnada.com",
    "dispostable.com",
];

/// Whether an address is on the list.
///
/// Sub-addressing (`user+tag@`) is *not* treated as disposable: it is how careful people track who
/// leaked their address, and punishing it annoys exactly the customers worth keeping. Only the
/// domain decides.
pub fn is_disposable(email: &str) -> bool {
    let Some((_, domain)) = email.rsplit_once('@') else {
        return false;
    };
    let domain = domain.trim().to_lowercase();
    DISPOSABLE_DOMAINS.iter().any(|d| {
        // A subdomain of a disposable host is disposable too — `foo.mailinator.com` is the
        // documented way to get an inbox there.
        domain == *d || domain.ends_with(&format!(".{d}"))
    })
}

/// A rate over a window, evaluated against a count.
///
/// Advisory by construction: it returns a verdict, and every caller decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Velocity {
    pub window_hours: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok {
        seen: i64,
    },
    /// Over the limit. Named `Suspicious` rather than `Abuse` because that is all it establishes.
    Suspicious {
        seen: i64,
        limit: i64,
    },
}

impl Verdict {
    pub fn is_suspicious(&self) -> bool {
        matches!(self, Verdict::Suspicious { .. })
    }
}

impl Velocity {
    pub fn judge(&self, seen: i64) -> Verdict {
        if seen > self.limit {
            Verdict::Suspicious {
                seen,
                limit: self.limit,
            }
        } else {
            Verdict::Ok { seen }
        }
    }
}

/// How many accounts were created in the window. The signup-flood signal.
pub async fn accounts_created(pool: &PgPool, window_hours: i64) -> Result<i64, AbuseError> {
    let (count,): (i64,) = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — a signup-flood check is about the population, not an
         -- account; the result is a count and names nobody.
         SELECT count(*)::bigint FROM accounts
         WHERE created_at > now() - make_interval(hours => $1::int)",
    )
    .bind(window_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(count)
}

/// How many keys one account minted in the window. A key-minting spike is either a compromised
/// dashboard session or a script, and both are worth a look.
pub async fn keys_issued(
    pool: &PgPool,
    account_id: Uuid,
    window_hours: i64,
) -> Result<i64, AbuseError> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT count(*)::bigint FROM api_keys
         WHERE account_id = $1 AND created_at > now() - make_interval(hours => $2::int)",
    )
    .bind(account_id)
    .bind(window_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(count)
}

/// What an account has spent in the window, in credit-micros (positive = spent).
///
/// The anomaly that matters commercially: an account whose burn rate jumps by an order of magnitude
/// is either a customer having a great week or a stolen key, and the difference is worth one look
/// from a person before the invoice.
pub async fn spend_micros(
    pool: &PgPool,
    account_id: Uuid,
    window_hours: i64,
) -> Result<i64, AbuseError> {
    let (spent,): (Option<i64>,) = sqlx::query_as(
        "SELECT -sum(amount_micros)::bigint FROM ledger_entries
         WHERE account_id = $1 AND amount_micros < 0
           AND at > now() - make_interval(hours => $2::int)",
    )
    .bind(account_id)
    .bind(window_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(spent.unwrap_or(0))
}

/// The kill switch (docs/17 §admin surface).
///
/// Immediate: `authenticate` refuses a suspended account's keys on the next request, without a
/// cache to invalidate. Reversible, and recorded — the audit row is written in the same transaction
/// as the suspension, so an action can never exist without its reason.
pub async fn suspend(
    pool: &PgPool,
    account_id: Uuid,
    actor: &str,
    reason: &str,
) -> Result<(), AbuseError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

    let done = sqlx::query(
        "UPDATE accounts SET suspended_at = now(), suspended_reason = $2 WHERE account_id = $1",
    )
    .bind(account_id)
    .bind(reason)
    .execute(&mut *tx)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    if done.rows_affected() == 0 {
        return Err(AbuseError::NoSuchAccount);
    }

    audit(&mut tx, account_id, "suspend", actor, Some(reason)).await?;
    tx.commit()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(())
}

pub async fn unsuspend(pool: &PgPool, account_id: Uuid, actor: &str) -> Result<(), AbuseError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

    let done = sqlx::query(
        "UPDATE accounts SET suspended_at = NULL, suspended_reason = NULL WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    if done.rows_affected() == 0 {
        return Err(AbuseError::NoSuchAccount);
    }

    // Reinstatement is audited too. "Who turned it back on" is the question an incident review asks
    // second, right after "who turned it off".
    audit(&mut tx, account_id, "unsuspend", actor, None).await?;
    tx.commit()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(())
}

/// Whether an account is suspended, and why.
pub async fn suspension(pool: &PgPool, account_id: Uuid) -> Result<Option<String>, AbuseError> {
    let row: Option<(Option<time::OffsetDateTime>, Option<String>)> =
        sqlx::query_as("SELECT suspended_at, suspended_reason FROM accounts WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| PgError::Query(e.to_string()))?;

    match row {
        Some((Some(_), reason)) => Ok(Some(reason.unwrap_or_else(|| "suspended".into()))),
        Some((None, _)) => Ok(None),
        None => Err(AbuseError::NoSuchAccount),
    }
}

/// One account's admin history, newest first.
pub async fn history(
    pool: &PgPool,
    account_id: Uuid,
    limit: i64,
) -> Result<Vec<(String, String, Option<String>)>, AbuseError> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT action, actor, reason FROM admin_actions
         WHERE account_id = $1 ORDER BY at DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(rows)
}

/// Record an administrative action. Append-only: there is no update or delete path.
pub async fn audit(
    tx: &mut sqlx::PgConnection,
    account_id: Uuid,
    action: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<(), AbuseError> {
    sqlx::query(
        "INSERT INTO admin_actions (action_id, account_id, action, actor, reason)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(action)
    .bind(actor)
    .bind(reason)
    .execute(&mut *tx)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disposable_list_is_a_list_and_not_a_guess() {
        assert!(is_disposable("someone@mailinator.com"));
        assert!(
            is_disposable("someone@MAILINATOR.COM"),
            "case does not decide"
        );
        // A subdomain is the documented way to get an inbox at some of these.
        assert!(is_disposable("someone@inbox.mailinator.com"));

        // The false positives a regex would produce, and which this must not.
        assert!(!is_disposable("someone@gmail.com"));
        assert!(!is_disposable("student@temp.university.edu"));
        assert!(!is_disposable("someone@mail.company.com"));
        assert!(!is_disposable("not-an-email"));
    }

    #[test]
    fn sub_addressing_is_not_suspicious() {
        // `user+tag@` is how careful people track who leaked their address. Punishing it annoys
        // exactly the customers worth keeping.
        assert!(!is_disposable("someone+panday@gmail.com"));
        assert!(
            is_disposable("someone+panday@mailinator.com"),
            "the domain still decides"
        );
    }

    #[test]
    fn a_velocity_verdict_is_a_signal_and_says_so() {
        let v = Velocity {
            window_hours: 1,
            limit: 20,
        };
        assert_eq!(
            v.judge(20),
            Verdict::Ok { seen: 20 },
            "at the limit is not over it"
        );
        assert!(v.judge(21).is_suspicious());
        // The verdict carries both numbers, because "31 in an hour, limit 20" is actionable and
        // "suspicious" is not.
        match v.judge(31) {
            Verdict::Suspicious { seen, limit } => {
                assert_eq!((seen, limit), (31, 20));
            }
            other => panic!("{other:?}"),
        }
    }
}
