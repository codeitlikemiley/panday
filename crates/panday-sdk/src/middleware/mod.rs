//! Composable `ModelClient` middleware (docs/10 Layer 2).
//!
//! ```ignore
//! let client = GatewayTransport::new(base_url, key)
//!     .with_timeout(Duration::from_secs(30))
//!     .with_retry(RetryPolicy::default());
//! ```
//!
//! ## Why decorators rather than `tower::Service`
//!
//! docs/10 sketched this stack as `tower::Service`. It is implemented as
//! `ModelClient` decorators instead, for a reason the streaming contract
//! forces:
//!
//! **Retry can only ever wrap the call that *establishes* the stream, never
//! the stream itself.** docs/11 is explicit — "Mid-stream failure after tokens
//! have flowed: emit `Error{retryable:true}` and let the harness decide (it
//! holds turn semantics; the gateway does not re-prompt on its own)." Once a
//! byte has been yielded, retrying would either duplicate tokens the user has
//! already seen or silently restart a turn the harness is mid-way through
//! accounting for.
//!
//! That makes the retry boundary exactly `ModelClient::chat` — a single
//! `async fn` returning `Result<ItemStream, _>`. `poll_ready`/`call` buys
//! nothing at that seam, and `tower`'s `Retry` would need the response to be
//! inspectable, which a boxed stream is not. Tower still belongs on the HTTP
//! ingress side (axum + tower, docs/02), which is where M11.5 will use it.

use crate::{ItemStream, ModelClient, PandayError};
use panday_types::model::ChatRequest;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

/// Longest server-stated `Retry-After` we will sleep inside a request.
///
/// The value comes off the wire (docs/25 M25.3) and nothing upstream bounds it.
/// Beyond this, a retry is not a retry — it is a hang — so the error is
/// returned instead, carrying the upstream's number for whoever can act on it.
pub const MAX_HONOURED_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Bounded exponential backoff with jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts *after* the first. `0` disables retrying.
    pub max_retries: u32,
    pub initial_backoff: Duration,
    /// Backoff is capped here; exponential growth is not allowed to run away.
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// Backoff before `attempt` (1-based), with jitter.
    ///
    /// Jitter is derived from the request id rather than an RNG: it needs to
    /// decorrelate concurrent callers, not to be unpredictable, and deriving
    /// it keeps the whole path deterministic under test — a retry schedule you
    /// cannot reproduce is one you cannot debug. It also avoids a `rand`
    /// dependency for something this small.
    pub fn backoff(&self, attempt: u32, jitter_seed: u128) -> Duration {
        let exp = self
            .initial_backoff
            .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
        let capped = exp.min(self.max_backoff);

        // Full jitter over [50%, 100%] of the capped delay: enough spread to
        // break thundering herds without ever collapsing the delay to zero.
        let millis = capped.as_millis() as u64;
        if millis == 0 {
            return capped;
        }
        let spread = millis / 2;
        let offset = if spread == 0 {
            0
        } else {
            (jitter_seed as u64).wrapping_mul(2_654_435_761) % (spread + 1)
        };
        Duration::from_millis(millis - spread + offset)
    }
}

/// Retries the *establishment* of a stream. See the module note.
pub struct Retry<C> {
    inner: C,
    policy: RetryPolicy,
}

impl<C> Retry<C> {
    pub fn new(inner: C, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

#[async_trait::async_trait]
impl<C: ModelClient> ModelClient for Retry<C> {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        // Jitter seed is per-request, so two concurrent calls that fail
        // together do not retry in lockstep.
        let seed = req.metadata.request.0.as_u128();
        let mut attempt = 0;

        loop {
            match self.inner.chat(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    // `is_retryable` is the single source of truth (docs/10:
                    // "Retryability is a method, not a guess"). Budget and
                    // entitlement failures are deliberately NOT retryable —
                    // hammering them wastes the caller's quota to no end.
                    if attempt >= self.policy.max_retries || !e.is_retryable() {
                        return Err(e);
                    }
                    attempt += 1;
                    // Honour a server-stated delay over our own curve — but
                    // only up to a ceiling. `Retry-After` is upstream-controlled
                    // and unbounded, and this sleep sits *outside* the timeout
                    // layer, so an absurd value would park the caller's future
                    // with no deadline. Past the ceiling, waiting costs more
                    // than failing over: hand the error back with the
                    // upstream's number intact and let the chain decide.
                    let stated = match &e {
                        PandayError::RateLimited { retry_after_ms } if *retry_after_ms > 0 => {
                            Some(Duration::from_millis(*retry_after_ms))
                        }
                        _ => None,
                    };
                    let wait = match stated {
                        Some(d) if d > MAX_HONOURED_RETRY_AFTER => return Err(e),
                        Some(d) => d,
                        None => self.policy.backoff(attempt, seed),
                    };
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

/// Bounds **time-to-first-stream**, not the stream's lifetime.
///
/// A long generation is not a hung one: a model streaming for two minutes is
/// working, while a gateway that has not answered the POST in ten seconds is
/// not. Wrapping the whole stream would cap the length of a legitimate
/// response, so the deadline covers only `chat()` returning.
pub struct Timeout<C> {
    inner: C,
    limit: Duration,
}

impl<C> Timeout<C> {
    pub fn new(inner: C, limit: Duration) -> Self {
        Self { inner, limit }
    }
}

#[async_trait::async_trait]
impl<C: ModelClient> ModelClient for Timeout<C> {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        match tokio::time::timeout(self.limit, self.inner.chat(req)).await {
            Ok(result) => result,
            // Retryable: a deadline says nothing about the request's validity,
            // so the next target in the chain deserves a turn.
            Err(_) => Err(PandayError::Provider {
                upstream: "transport".into(),
                message: format!("no response within {:?}", self.limit),
                retryable: true,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Builder sugar
// ---------------------------------------------------------------------------

/// Fluent layering for any `ModelClient`.
///
/// Order matters: `.with_timeout(..).with_retry(..)` puts retry *outside* the
/// timeout, so each attempt gets its own deadline. The reverse would let one
/// deadline cover every attempt, which is almost never what you want.
pub trait ModelClientExt: ModelClient + Sized {
    fn with_retry(self, policy: RetryPolicy) -> Retry<Self> {
        Retry::new(self, policy)
    }
    fn with_timeout(self, limit: Duration) -> Timeout<Self> {
        Timeout::new(self, limit)
    }
}

impl<C: ModelClient + Sized> ModelClientExt for C {}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_types::id::{AccountId, RequestId};
    use panday_types::model::{CallMeta, ContentBlock, Message, ModelRef, Role, Sampling};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn a_request() -> ChatRequest {
        ChatRequest {
            model: ModelRef("local/m".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling::default(),
            cache: Default::default(),
            stream: true,
            metadata: CallMeta {
                account: AccountId::new(),
                request: RequestId::new(),
                session: None,
                turn: None,
                task: None,
            },
        }
    }

    fn empty_stream() -> ItemStream {
        Box::pin(futures_util::stream::empty())
    }

    /// Fails a set number of times, then succeeds; counts every attempt.
    struct Flaky {
        fail_times: u32,
        error: fn() -> PandayError,
        attempts: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl ModelClient for Flaky {
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                Err((self.error)())
            } else {
                Ok(empty_stream())
            }
        }
    }

    fn flaky(fail_times: u32, error: fn() -> PandayError) -> (Flaky, Arc<AtomicU32>) {
        let attempts = Arc::new(AtomicU32::new(0));
        (
            Flaky {
                fail_times,
                error,
                attempts: attempts.clone(),
            },
            attempts,
        )
    }

    fn retryable() -> PandayError {
        PandayError::Provider {
            upstream: "test".into(),
            message: "503".into(),
            retryable: true,
        }
    }

    fn fatal() -> PandayError {
        PandayError::EntitlementDenied {
            plan: "free".into(),
            needed: "frontier".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_a_retryable_failure_then_succeeds() {
        let (inner, attempts) = flaky(2, retryable);
        let client = inner.with_retry(RetryPolicy::default());

        assert!(client.chat(a_request()).await.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "1 initial + 2 retries");
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_retries_and_returns_the_last_error() {
        let (inner, attempts) = flaky(99, retryable);
        let client = inner.with_retry(RetryPolicy {
            max_retries: 2,
            ..Default::default()
        });

        let err = client.chat(a_request()).await.map(|_| ()).unwrap_err();
        assert!(err.is_retryable());
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "must not retry forever");
    }

    #[tokio::test(start_paused = true)]
    async fn never_retries_a_non_retryable_error() {
        // Retrying an entitlement denial burns the caller's quota for nothing.
        let (inner, attempts) = flaky(99, fatal);
        let client = inner.with_retry(RetryPolicy::default());

        let err = client.chat(a_request()).await.map(|_| ()).unwrap_err();
        assert!(matches!(err, PandayError::EntitlementDenied { .. }));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "exactly one attempt");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_max_retries_disables_retrying() {
        let (inner, attempts) = flaky(99, retryable);
        let client = inner.with_retry(RetryPolicy {
            max_retries: 0,
            ..Default::default()
        });

        assert!(client.chat(a_request()).await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_grows_exponentially_and_is_capped() {
        let p = RetryPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
        };
        // Jitter spans [50%, 100%], so compare against the band, not a point.
        let within = |d: Duration, base: u64| {
            let ms = d.as_millis() as u64;
            ms >= base / 2 && ms <= base
        };
        assert!(within(p.backoff(1, 0), 100));
        assert!(within(p.backoff(2, 0), 200));
        assert!(within(p.backoff(3, 0), 400));
        // Capped from here on, never 800.
        assert!(within(p.backoff(4, 0), 400));
        assert!(within(p.backoff(9, 0), 400));
    }

    #[test]
    fn jitter_decorrelates_different_requests() {
        let p = RetryPolicy::default();
        let a = p.backoff(3, 1);
        let b = p.backoff(3, 2);
        let c = p.backoff(3, 3);
        assert!(
            a != b || b != c,
            "identical delays across requests would defeat the purpose"
        );
    }

    #[test]
    fn jitter_is_deterministic_for_one_request() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff(2, 42), p.backoff(2, 42), "must be reproducible");
    }

    /// Never returns — stands in for a gateway that accepted the connection
    /// and then went silent.
    struct Hangs;

    #[async_trait::async_trait]
    impl ModelClient for Hangs {
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            futures_util::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_fires_when_the_stream_never_establishes() {
        let client = Hangs.with_timeout(Duration::from_secs(5));
        let err = client.chat(a_request()).await.map(|_| ()).unwrap_err();
        assert!(
            err.is_retryable(),
            "a deadline says nothing about validity; the next target deserves a turn"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_outside_timeout_gives_each_attempt_its_own_deadline() {
        // Composition check: a permanently hanging inner client must be
        // attempted (and time out) once per allowed try, not once overall.
        struct CountingHang(Arc<AtomicU32>);

        #[async_trait::async_trait]
        impl ModelClient for CountingHang {
            async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                futures_util::future::pending().await
            }
        }

        let attempts = Arc::new(AtomicU32::new(0));
        let client = CountingHang(attempts.clone())
            .with_timeout(Duration::from_secs(1))
            .with_retry(RetryPolicy {
                max_retries: 2,
                ..Default::default()
            });

        assert!(client.chat(a_request()).await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "1 initial + 2 retries");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_stated_retry_after_wins_over_our_curve() {
        // If the server told us how long to wait, ignoring it is rude and
        // gets us rate-limited harder.
        let p = RetryPolicy {
            max_retries: 1,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
        };
        let (inner, attempts) = flaky(1, || PandayError::RateLimited {
            retry_after_ms: 30_000,
        });

        let start = tokio::time::Instant::now();
        assert!(inner.with_retry(p).chat(a_request()).await.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            start.elapsed() >= Duration::from_millis(30_000),
            "must honour the server's stated delay, not our 1ms curve"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_absurd_retry_after_fails_over_instead_of_parking_the_caller() {
        // `Retry-After` is upstream-controlled and this sleep is outside the
        // timeout layer, so an unbounded value would be a hang, not a retry.
        let p = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
        };
        let absurd_ms = MAX_HONOURED_RETRY_AFTER.as_millis() as u64 + 1_000;
        let (inner, attempts) = flaky(u32::MAX, || PandayError::RateLimited {
            retry_after_ms: 999_999_999_000,
        });

        let start = tokio::time::Instant::now();
        let err = inner
            .with_retry(p)
            .chat(a_request())
            .await
            .err()
            .expect("gives up rather than sleeping");

        match err {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                retry_after_ms, 999_999_999_000,
                "the upstream's number survives for whoever can act on it"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no second attempt");
        assert!(
            start.elapsed() < Duration::from_millis(absurd_ms),
            "must not sleep the stated delay"
        );
    }
}
