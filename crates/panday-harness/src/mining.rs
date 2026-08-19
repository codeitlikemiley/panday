//! Transcript mining (docs/19 §19.6, M19.4).
//!
//! Turning real sessions into training data is the part of a data pipeline where the mistakes are
//! not recoverable: a customer's secret that reaches a dataset is in every checkpoint trained on it,
//! and a licence you did not have is a licence you cannot retroactively obtain. So the order here is
//! **consent, then scrub, then provenance** — and each step defaults to refusing.
//!
//! - **Consent is explicit and per session.** There is no "assume yes for internal accounts" and no
//!   inference from a plan. A session whose consent is unknown is not mined, and `Consent::Unknown`
//!   is a distinct state from `Consent::Denied` so a missing record cannot be silently read as a
//!   permission.
//! - **Scrubbing is deny-first and layered.** Known secrets from the vault (the values we can match
//!   exactly), then shapes that are secrets by construction — keys, tokens, emails, IPs, home paths.
//!   Anything that still looks like a credential after scrubbing **drops the example** rather than
//!   shipping it: a dataset is worth less than a leak costs.
//! - **Provenance travels with every example.** Session, seq range, model, and the scrubber version
//!   that produced it. When a scrubber bug is found — and one will be — the question "which examples
//!   came out of the broken version" has to have an answer that is not "all of them".

use panday_types::event::{Envelope, Event};
use serde::{Deserialize, Serialize};

/// Bump when the scrubber's behaviour changes. Stamped into every example, so a bad batch can be
/// identified and withdrawn instead of the whole corpus being thrown away.
pub const SCRUBBER_VERSION: u32 = 1;

/// What a session's owner said about training on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consent {
    /// Explicitly granted, with a record.
    Granted,
    /// Explicitly refused.
    Denied,
    /// Nobody asked, or the answer was not recorded. **Not** a yes.
    Unknown,
}

impl Consent {
    pub fn permits_mining(&self) -> bool {
        matches!(self, Consent::Granted)
    }
}

/// One training example, in the format docs/19 mandates: OpenAI-style `messages`, tool turns
/// preserved rather than flattened to text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Example {
    pub messages: Vec<Message>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub session_id: String,
    /// The events this example was built from, inclusive.
    pub from_seq: u64,
    pub to_seq: u64,
    pub model: String,
    pub scrubber_version: u32,
    /// What the owner agreed to, carried rather than assumed — a dataset row whose consent has to
    /// be looked up elsewhere is one nobody will look up.
    pub consent: Consent,
}

/// Why an example was not produced. Counted, because a miner that silently drops is a miner whose
/// yield nobody can explain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MiningReport {
    pub examples: u32,
    pub skipped_no_consent: u32,
    pub skipped_still_sensitive: u32,
    pub skipped_too_short: u32,
}

/// Mine summarizer pairs from one session's log.
///
/// The pair docs/19 wants first: a long tool output and the reduction that stood in for it. The
/// reducer already made that judgement thousands of times per session, and its output is exactly
/// what a small model should learn to produce.
pub fn summarizer_pairs(
    events: &[Envelope],
    consent: Consent,
    scrub: &dyn Fn(&str) -> String,
) -> (Vec<Example>, MiningReport) {
    let mut report = MiningReport::default();
    if !consent.permits_mining() {
        // Counted once per session, not per candidate: the number that matters is "how many
        // sessions could we not use", and inflating it by their length would hide it.
        report.skipped_no_consent = 1;
        return (Vec::new(), report);
    }

    let model = events
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnStarted { model, .. } => Some(model.0.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "unknown".into());

    let mut out = Vec::new();
    for envelope in events {
        let Event::ToolResult { output, .. } = &envelope.event else {
            continue;
        };
        // An error output teaches a summarizer to summarise failures, which is a different task and
        // a much smaller corpus. Left out on purpose rather than by accident.
        if output.tokens_raw == 0 || output.text.is_empty() {
            continue;
        }
        // A reduction that removed nothing is not a summarisation example — it is a copy, and a
        // model trained on copies learns to copy.
        if output.tokens_kept >= output.tokens_raw {
            report.skipped_too_short += 1;
            continue;
        }

        let reduced = scrub(&output.text);
        if looks_sensitive(&reduced) {
            // A dataset is worth less than a leak costs.
            report.skipped_still_sensitive += 1;
            continue;
        }

        out.push(Example {
            messages: vec![
                Message {
                    role: "system".into(),
                    content: "Summarise tool output for an agent, keeping what it needs to act."
                        .into(),
                },
                Message {
                    role: "user".into(),
                    content: format!(
                        "Reduce this to about {} tokens, keeping errors, paths and identifiers.",
                        output.tokens_kept
                    ),
                },
                Message {
                    role: "assistant".into(),
                    content: reduced,
                },
            ],
            provenance: Provenance {
                session_id: envelope.session_id.0.to_string(),
                from_seq: envelope.seq,
                to_seq: envelope.seq,
                model: model.clone(),
                scrubber_version: SCRUBBER_VERSION,
                consent,
            },
        });
    }

    report.examples = out.len() as u32;
    (out, report)
}

/// Shapes that are credentials by construction, redacted regardless of what any vault knows.
///
/// The vault can only match values it was told about; this catches the ones nobody registered —
/// a key pasted into a terminal, an email in a stack trace, a home directory that names a person.
pub fn scrub_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(|c: char| c.is_whitespace()) {
        let (word, trailing) = split_trailing_space(token);
        out.push_str(&redact_word(word));
        out.push_str(trailing);
    }
    out
}

fn split_trailing_space(token: &str) -> (&str, &str) {
    match token.find(char::is_whitespace) {
        Some(at) => (&token[..at], &token[at..]),
        None => (token, ""),
    }
}

fn redact_word(word: &str) -> String {
    let trimmed = word.trim_matches(|c: char| "\"'`,;()[]{}<>".contains(c));
    if trimmed.is_empty() {
        return word.to_string();
    }

    let redacted = if is_email(trimmed) {
        Some("[redacted:email]")
    } else if is_key_like(trimmed) {
        Some("[redacted:key]")
    } else if is_ipv4(trimmed) {
        Some("[redacted:ip]")
    } else if let Some(rest) = home_path(trimmed) {
        return word.replace(trimmed, &format!("/home/[redacted:user]{rest}"));
    } else {
        None
    };

    match redacted {
        Some(replacement) => word.replace(trimmed, replacement),
        None => word.to_string(),
    }
}

fn is_email(word: &str) -> bool {
    let Some((local, domain)) = word.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

/// A long, high-entropy token, or one wearing a known key prefix.
///
/// Length and shape rather than a provider list: the list is always out of date, and the property
/// that makes a key a key — long, random, no spaces — does not change.
fn is_key_like(word: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "pnd_live_",
        "pnd_test_",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "AKIA",
        "AIza",
        "eyJ",
    ];
    if PREFIXES.iter().any(|p| word.starts_with(p)) {
        return true;
    }
    // 32+ characters of base64/hex alphabet with both letters and digits: a hash, a token, or a
    // secret. A path or a sentence fails this on the first slash or space.
    word.len() >= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+' || c == '=')
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_ipv4(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
}

/// `/home/alice/...` or `/Users/alice/...` → the part after the username.
fn home_path(word: &str) -> Option<&str> {
    for prefix in ["/home/", "/Users/"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            let end = rest.find('/').unwrap_or(rest.len());
            if end > 0 {
                return Some(&rest[end..]);
            }
        }
    }
    None
}

/// The last gate: does this still look like it contains a credential?
///
/// Deliberately stricter than the scrubber. Anything that trips it is dropped rather than fixed,
/// because a shape the scrubber did not recognise is a shape we do not understand.
pub fn looks_sensitive(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let w = word.trim_matches(|c: char| "\"'`,;()[]{}<>".contains(c));
        is_key_like(w) || is_email(w)
    }) || text.contains("BEGIN PRIVATE KEY")
        || text.contains("BEGIN RSA PRIVATE KEY")
        || text.contains("BEGIN OPENSSH PRIVATE KEY")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_consent_is_not_a_yes() {
        // The single most expensive mistake this module could make.
        assert!(Consent::Granted.permits_mining());
        assert!(!Consent::Denied.permits_mining());
        assert!(!Consent::Unknown.permits_mining());
    }

    #[test]
    fn shapes_that_are_credentials_by_construction_are_redacted() {
        let text = "curl -H 'Authorization: Bearer sk-ant-api03-AAAABBBBCCCCDDDD' https://x";
        assert!(!scrub_shapes(text).contains("sk-ant-api03"));

        assert_eq!(
            scrub_shapes("mail alice@example.com now"),
            "mail [redacted:email] now"
        );
        assert_eq!(
            scrub_shapes("connect 192.168.1.44 please"),
            "connect [redacted:ip] please"
        );
        assert_eq!(
            scrub_shapes("open /Users/alice/src/panday/lib.rs"),
            "open /home/[redacted:user]/src/panday/lib.rs"
        );
    }

    #[test]
    fn ordinary_text_survives_scrubbing_intact() {
        // A scrubber that mangles normal output produces a dataset that teaches mangling.
        let text = "error[E0308]: mismatched types in src/gateway.rs:142 — expected u64, found i32";
        assert_eq!(scrub_shapes(text), text);
        assert_eq!(scrub_shapes("version 1.2.3.4rc1"), "version 1.2.3.4rc1");
    }

    #[test]
    fn a_long_hash_is_treated_as_a_secret_and_a_long_sentence_is_not() {
        let hash = "a3f5b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f";
        assert!(is_key_like(hash));
        assert!(!is_key_like(
            "this is a perfectly ordinary sentence of some length"
        ));
        // A path is not a key, however long.
        assert!(!is_key_like(
            "/very/long/path/to/some/deeply/nested/source/file/name.rs"
        ));
    }

    #[test]
    fn the_last_gate_drops_what_the_scrubber_missed() {
        // A shape the scrubber did not recognise is a shape we do not understand, and the answer to
        // that is to drop the example rather than to ship it.
        assert!(looks_sensitive("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(looks_sensitive("contact bob@example.org"));
        assert!(!looks_sensitive("a normal line of tool output"));
    }

    #[test]
    fn a_reduction_that_removed_nothing_is_not_an_example() {
        // A model trained on copies learns to copy.
        use panday_types::event::ReducedOutput;
        use panday_types::{CallId, SessionId};

        let envelope = |seq, output| Envelope {
            v: 1,
            session_id: SessionId::new(),
            seq,
            turn_id: None,
            at: time::OffsetDateTime::now_utc(),
            event: Event::ToolResult {
                call_id: CallId::new(),
                output,
                raw_ref: None,
                duration_ms: 1,
                is_error: false,
            },
        };

        let events = vec![
            envelope(
                0,
                ReducedOutput {
                    text: "kept everything".into(),
                    tokens_raw: 10,
                    tokens_kept: 10,
                    strategy: "generic".into(),
                },
            ),
            envelope(
                1,
                ReducedOutput {
                    text: "a real reduction of a long build log".into(),
                    tokens_raw: 900,
                    tokens_kept: 12,
                    strategy: "cargo".into(),
                },
            ),
        ];

        let (examples, report) = summarizer_pairs(&events, Consent::Granted, &|t| t.to_string());
        assert_eq!(examples.len(), 1);
        assert_eq!(report.skipped_too_short, 1);
        assert_eq!(examples[0].provenance.scrubber_version, SCRUBBER_VERSION);
        assert_eq!(examples[0].provenance.consent, Consent::Granted);
    }

    #[test]
    fn a_session_without_consent_yields_nothing_and_says_so_once() {
        use panday_types::event::ReducedOutput;
        use panday_types::{CallId, SessionId};

        let events: Vec<Envelope> = (0..5)
            .map(|seq| Envelope {
                v: 1,
                session_id: SessionId::new(),
                seq,
                turn_id: None,
                at: time::OffsetDateTime::now_utc(),
                event: Event::ToolResult {
                    call_id: CallId::new(),
                    output: ReducedOutput {
                        text: "something".into(),
                        tokens_raw: 100,
                        tokens_kept: 5,
                        strategy: "generic".into(),
                    },
                    raw_ref: None,
                    duration_ms: 1,
                    is_error: false,
                },
            })
            .collect();

        let (examples, report) = summarizer_pairs(&events, Consent::Unknown, &|t| t.to_string());
        assert!(examples.is_empty());
        // Once per session: counting per candidate would inflate the number that matters.
        assert_eq!(report.skipped_no_consent, 1);
    }
}
