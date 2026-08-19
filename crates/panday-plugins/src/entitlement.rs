//! Offline entitlement tokens (docs/17 §Enterprise / offline licensing, M17.6).
//!
//! > "Signed **entitlement tokens** (ed25519, ~90-day expiry + grace): the offline daemon (18)
//! > validates the signature locally — no phone-home required, renewal is a file. Seat counting is
//! > honest-declaration + audit log; don't build spyware."
//!
//! Three decisions this module makes, all of them consequences of that last clause:
//!
//! - **Verification is local and offline, always.** There is no revocation check, no call home, no
//!   activation. A licence that stops working because a network is down is a licence that fails
//!   exactly when the offline tier is most valuable.
//! - **Expiry degrades, it does not brick.** Past the grace window the token stops granting; the
//!   software keeps working at the community tier (ADR-011: the free offline tier needs no account
//!   at all). Bricking a paying customer's laptop over a renewal e-mail is not a business model, it
//!   is an outage you charged for.
//! - **Seats are a declaration, not an enforcement.** The token says how many were bought. Nothing
//!   here counts machines, phones home, or fingerprints hardware — the audit trail is the customer's
//!   own honesty plus their contract. docs/17 calls the alternative spyware, and it is right.
//!
//! It lives in `panday-plugins` because that is where signed artifacts already live: one ed25519
//! dependency, one verification path, one file to review when the crypto matters.

use crate::signature::{verify_from_trusted_key, SignatureError, SigningKeyPair};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// What was bought.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entitlement {
    pub version: u32,
    /// Who it is for, in whatever form the contract uses — an account id, or a company name for an
    /// air-gapped customer who never had an account (docs/18 M18.7).
    pub subject: String,
    pub plan: String,
    /// Declared, never enforced. See the module note.
    pub seats: u32,
    /// RFC 3339.
    pub issued_at: String,
    pub expires_at: String,
    /// How long past expiry the token keeps granting, loudly. A renewal that arrives on the day is
    /// a renewal that has already been late once.
    pub grace_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where a token stands right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Valid. `days_left` so a UI can start reminding before it is urgent.
    Active { days_left: i64 },
    /// Expired, still granting, and the customer should be told every single time.
    Grace { days_left: i64 },
    /// Past grace. Grants nothing; the software falls back to the community tier.
    Expired { days_ago: i64 },
    /// Issued in the future by more than a plausible clock skew.
    NotYetValid,
}

impl Status {
    /// Whether the entitlement grants anything right now.
    pub fn grants(&self) -> bool {
        matches!(self, Status::Active { .. } | Status::Grace { .. })
    }

    /// What a human should be told, if anything.
    pub fn warning(&self) -> Option<String> {
        match self {
            Status::Active { days_left } if *days_left <= 14 => Some(format!(
                "licence expires in {days_left} days — renewal is a file, not a phone call"
            )),
            Status::Active { .. } => None,
            Status::Grace { days_left } => Some(format!(
                "licence EXPIRED and is running on grace for {days_left} more days"
            )),
            Status::Expired { days_ago } => Some(format!(
                "licence expired {days_ago} days ago; running at the community tier"
            )),
            Status::NotYetValid => {
                Some("licence is dated in the future; check this machine's clock".into())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EntitlementError {
    #[error("signature: {0}")]
    Signature(#[from] SignatureError),
    #[error("parse: {0}")]
    Parse(String),
    #[error("entitlement version {0} is not supported")]
    Version(u32),
    #[error("`{field}` is not an RFC 3339 timestamp")]
    Timestamp { field: &'static str },
}

/// Tolerance for a machine whose clock is wrong.
///
/// An air-gapped box with a dead RTC is a real thing, and refusing a valid licence because a laptop
/// thinks it is Tuesday is the kind of failure that gets a product ripped out. A day is enough for
/// skew and not enough to be worth attacking — and moving a clock back to extend a licence works
/// with or without this, which is why expiry is a business control rather than a security one.
pub const CLOCK_SKEW_DAYS: i64 = 1;

/// Verify a token and say where it stands, without asking anything on the network.
pub fn verify(
    document: &[u8],
    signature_hex: &str,
    trusted_key_hex: &str,
    now: OffsetDateTime,
) -> Result<(Entitlement, Status), EntitlementError> {
    // Signature first, over the bytes as received: a token whose contents were parsed before they
    // were authenticated has already been trusted.
    verify_from_trusted_key(document, signature_hex, trusted_key_hex)?;

    let entitlement: Entitlement =
        serde_json::from_slice(document).map_err(|e| EntitlementError::Parse(e.to_string()))?;
    if entitlement.version != 1 {
        return Err(EntitlementError::Version(entitlement.version));
    }

    let status = status_at(&entitlement, now)?;
    Ok((entitlement, status))
}

/// Where a token stands at a given moment. Separate from `verify` so expiry can be tested without
/// keys, and so a UI can ask "what about next Tuesday".
pub fn status_at(
    entitlement: &Entitlement,
    now: OffsetDateTime,
) -> Result<Status, EntitlementError> {
    let issued = parse(&entitlement.issued_at, "issued_at")?;
    let expires = parse(&entitlement.expires_at, "expires_at")?;

    if now < issued - time::Duration::days(CLOCK_SKEW_DAYS) {
        return Ok(Status::NotYetValid);
    }
    if now <= expires {
        return Ok(Status::Active {
            days_left: (expires - now).whole_days(),
        });
    }

    let grace_ends = expires + time::Duration::days(entitlement.grace_days as i64);
    if now <= grace_ends {
        return Ok(Status::Grace {
            days_left: (grace_ends - now).whole_days(),
        });
    }
    Ok(Status::Expired {
        days_ago: (now - expires).whole_days(),
    })
}

/// Issue and sign. The platform side; kept next to verification so the two cannot drift.
pub fn issue(keys: &SigningKeyPair, entitlement: &Entitlement) -> Result<(String, String), String> {
    let document = serde_json::to_string_pretty(entitlement).map_err(|e| e.to_string())?;
    let signature = keys.sign_archive(document.as_bytes());
    Ok((document, signature))
}

fn parse(value: &str, field: &'static str) -> Result<OffsetDateTime, EntitlementError> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| EntitlementError::Timestamp { field })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
    }

    fn entitlement() -> Entitlement {
        Entitlement {
            version: 1,
            subject: "acme".into(),
            plan: "enterprise".into(),
            seats: 25,
            issued_at: "2026-01-01T00:00:00Z".into(),
            expires_at: "2026-04-01T00:00:00Z".into(),
            grace_days: 30,
            note: None,
        }
    }

    #[test]
    fn a_current_token_is_active_and_says_how_long_is_left() {
        let s = status_at(&entitlement(), at("2026-03-01T00:00:00Z")).unwrap();
        assert_eq!(s, Status::Active { days_left: 31 });
        assert!(s.grants());
        assert!(
            s.warning().is_none(),
            "no nagging while there is a month left"
        );
    }

    #[test]
    fn the_warning_starts_before_it_is_urgent() {
        let s = status_at(&entitlement(), at("2026-03-25T00:00:00Z")).unwrap();
        assert!(s.grants());
        assert!(s.warning().unwrap().contains("expires in 7 days"));
    }

    #[test]
    fn grace_still_grants_and_says_so_every_time() {
        // A renewal that arrives on the day is a renewal that has already been late once.
        let s = status_at(&entitlement(), at("2026-04-15T00:00:00Z")).unwrap();
        assert_eq!(s, Status::Grace { days_left: 16 });
        assert!(s.grants());
        assert!(s.warning().unwrap().contains("EXPIRED"));
    }

    #[test]
    fn past_grace_it_stops_granting_but_nothing_breaks() {
        // The software keeps working at the community tier. Bricking a paying customer's laptop
        // over a renewal e-mail is an outage you charged for.
        let s = status_at(&entitlement(), at("2026-06-01T00:00:00Z")).unwrap();
        assert!(matches!(s, Status::Expired { .. }));
        assert!(!s.grants());
        assert!(s.warning().unwrap().contains("community tier"));
    }

    #[test]
    fn a_clock_that_is_slightly_wrong_does_not_void_a_licence() {
        // An air-gapped box with a dead RTC is a real thing.
        let e = entitlement();
        assert!(status_at(&e, at("2025-12-31T12:00:00Z")).unwrap().grants());
        assert_eq!(
            status_at(&e, at("2025-06-01T00:00:00Z")).unwrap(),
            Status::NotYetValid
        );
    }

    #[test]
    fn a_token_round_trips_through_issue_and_verify() {
        let keys = SigningKeyPair::from_bytes(&[3u8; 32]);
        let (document, signature) = issue(&keys, &entitlement()).unwrap();
        let (back, status) = verify(
            document.as_bytes(),
            &signature,
            &keys.public_key_hex(),
            at("2026-03-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(back, entitlement());
        assert!(status.grants());
    }

    #[test]
    fn editing_the_expiry_invalidates_the_token() {
        // The whole point: a text file a customer can read is also one they can edit.
        let keys = SigningKeyPair::from_bytes(&[3u8; 32]);
        let (document, signature) = issue(&keys, &entitlement()).unwrap();
        let tampered = document.replace("2026-04-01", "2036-04-01");

        assert!(matches!(
            verify(
                tampered.as_bytes(),
                &signature,
                &keys.public_key_hex(),
                at("2026-03-01T00:00:00Z")
            ),
            Err(EntitlementError::Signature(_))
        ));
    }

    #[test]
    fn a_token_signed_by_someone_else_is_not_a_token() {
        let theirs = SigningKeyPair::from_bytes(&[9u8; 32]);
        let (document, signature) = issue(&theirs, &entitlement()).unwrap();
        let ours = SigningKeyPair::from_bytes(&[3u8; 32]);
        assert!(verify(
            document.as_bytes(),
            &signature,
            &ours.public_key_hex(),
            at("2026-03-01T00:00:00Z")
        )
        .is_err());
    }

    #[test]
    fn seats_are_carried_and_nothing_counts_them() {
        // Stated as a test because it is a product decision that would otherwise erode: the day
        // something in this crate starts counting machines, this test is what has to be deleted.
        let (e, _) = {
            let keys = SigningKeyPair::from_bytes(&[3u8; 32]);
            let (d, s) = issue(&keys, &entitlement()).unwrap();
            verify(
                d.as_bytes(),
                &s,
                &keys.public_key_hex(),
                at("2026-03-01T00:00:00Z"),
            )
            .unwrap()
        };
        assert_eq!(e.seats, 25);
    }
}
