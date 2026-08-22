//! The HTTP seam.
//!
//! The dialect (`wire`) and the stream machine (`ChunkTranslator`) are pure;
//! this is the only place that knows a socket exists. Isolating it is what
//! lets the whole adapter be tested without a model running — tests supply a
//! `MockTransport` and the suite never opens a connection.

use crate::PandayError;
use futures_core::Stream;
use std::pin::Pin;
use std::time::Duration;

/// A stream of raw response-body chunks. Chunk boundaries are arbitrary and
/// may split a line or a UTF-8 sequence; `SseDecoder` handles that.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, PandayError>> + Send>>;

/// Upstream response headers the transport kept (docs/25 M25.3).
///
/// Names are stored lowercased. Values that are not UTF-8 are dropped rather
/// than lossy-decoded: a header we cannot read is a header we must not guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHeaders {
    pairs: Vec<(String, String)>,
}

impl ResponseHeaders {
    /// Build from `(name, value)` pairs. Names are lowercased.
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            pairs: pairs
                .into_iter()
                .map(|(n, v)| (n.into().to_ascii_lowercase(), v.into()))
                .collect(),
        }
    }

    /// Copy a reqwest `HeaderMap`. Invalid UTF-8 values are skipped.
    pub fn from_header_map(map: &reqwest::header::HeaderMap) -> Self {
        Self::from_pairs(map.iter().filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        }))
    }

    /// Case-insensitive lookup. First match wins.
    pub fn get(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.pairs
            .iter()
            .find(|(n, _)| n == &want)
            .map(|(_, v)| v.as_str())
    }

    /// `Retry-After` as milliseconds. Delta-seconds (`"5"` → 5000) or an
    /// IMF-fixdate in the future. Absent or unparseable → `None`.
    pub fn retry_after_ms(&self) -> Option<u64> {
        parse_retry_after_ms(self.get("retry-after")?)
    }

    /// Remaining counters named in docs/25. OpenAI / xAI use
    /// `x-ratelimit-remaining-*`; Anthropic uses `anthropic-ratelimit-*-remaining`.
    /// Overlaying these onto operator % is M25.7; this only stops dropping them.
    pub fn ratelimit_remaining(&self) -> RatelimitRemaining {
        RatelimitRemaining {
            requests: self
                .get_u64("x-ratelimit-remaining-requests")
                .or_else(|| self.get_u64("anthropic-ratelimit-requests-remaining")),
            tokens: self
                .get_u64("x-ratelimit-remaining-tokens")
                .or_else(|| self.get_u64("anthropic-ratelimit-tokens-remaining")),
            limit_requests: self
                .get_u64("x-ratelimit-limit-requests")
                .or_else(|| self.get_u64("anthropic-ratelimit-requests-limit")),
            limit_tokens: self
                .get_u64("x-ratelimit-limit-tokens")
                .or_else(|| self.get_u64("anthropic-ratelimit-tokens-limit")),
        }
    }

    fn get_u64(&self, name: &str) -> Option<u64> {
        self.get(name)?.trim().parse().ok()
    }
}

/// Remaining quota the upstream put on the wire. `None` means the header was
/// absent, not that remaining is zero.
///
/// Carries the **limits** as well as the remainders, because a remaining count
/// on its own is not a percentage: "412 requests left" says nothing about
/// headroom until you know whether the ceiling is 500 or 500,000. M25.3 kept
/// only the remainders, which was enough to stop dropping them and not enough
/// to overlay anything (docs/25 M25.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RatelimitRemaining {
    pub requests: Option<u64>,
    pub tokens: Option<u64>,
    pub limit_requests: Option<u64>,
    pub limit_tokens: Option<u64>,
}

impl RatelimitRemaining {
    /// Fraction of the provider's short window still available, by whichever of
    /// requests or tokens is *scarcer* — a credential with 90% of its requests
    /// and 3% of its tokens left has 3% of headroom, and reporting the kinder
    /// number would hide the one about to bite.
    ///
    /// `None` unless the upstream sent both a remaining and a limit for at
    /// least one of them.
    pub fn headroom_pct(&self) -> Option<f64> {
        let pair = |rem: Option<u64>, lim: Option<u64>| match (rem, lim) {
            (Some(r), Some(l)) if l > 0 => Some(r.min(l) as f64 / l as f64),
            _ => None,
        };
        match (
            pair(self.requests, self.limit_requests),
            pair(self.tokens, self.limit_tokens),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// True when the upstream told us anything at all.
    pub fn is_present(&self) -> bool {
        self.requests.is_some() || self.tokens.is_some()
    }
}

/// Somewhere to report the ratelimit headers a call came back with.
///
/// A push, not a getter. The value is produced deep in the adapter and consumed
/// by whoever owns the *credential* — and only the caller knows which credential
/// this adapter is. A `last_remaining()` getter would also race: two concurrent
/// calls on one credential would overwrite each other and the reader could not
/// tell which answer it got.
pub trait RemainingSink: Send + Sync {
    fn observe(&self, remaining: RatelimitRemaining);
}

/// A successful SSE POST: body stream **and** the headers that arrived with it.
pub struct SseResponse {
    pub headers: ResponseHeaders,
    pub body: ByteStream,
}

/// Issues one streaming POST and hands back the body **and** response headers.
///
/// Deliberately narrow: no retries, no timeouts, no auth policy. Retry and
/// timeout are middleware (`crate::middleware`), and *which* headers carry
/// credentials is the dialect's business — Chat Completions uses
/// `Authorization: Bearer`, Anthropic uses `x-api-key` plus a required
/// `anthropic-version`. So the caller supplies headers and this just sends
/// them.
#[async_trait::async_trait]
pub trait HttpStreamTransport: Send + Sync {
    async fn post_sse(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<SseResponse, PandayError>;

    /// One buffered GET. Used to list models; mocks that never implement it
    /// fail closed rather than inventing an inventory.
    async fn get_json(
        &self,
        _url: &str,
        _headers: &[(String, String)],
    ) -> Result<Vec<u8>, PandayError> {
        Err(PandayError::Protocol(
            "this transport does not implement GET".into(),
        ))
    }
}

/// The production transport.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

/// Map a transport failure to the IR error vocabulary.
///
/// Retryability is decided here, once, from what the wire actually said —
/// docs/10: "Retryability is a method, not a guess." A connect/timeout failure
/// is retryable; a malformed request is not.
fn transport_error(e: reqwest::Error) -> PandayError {
    let retryable = e.is_timeout() || e.is_connect() || e.is_request();
    PandayError::Provider {
        upstream: "http".into(),
        message: e.to_string(),
        retryable,
    }
}

/// Map a non-2xx response to the IR error vocabulary.
fn status_error(
    status: reqwest::StatusCode,
    body: String,
    headers: &ResponseHeaders,
) -> PandayError {
    // 429 carries its own type so the gateway's failover can back off rather
    // than burn the next provider in the chain. `Retry-After` is the wait;
    // missing it stays 0 (today's behaviour) rather than inventing a delay.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return PandayError::RateLimited {
            retry_after_ms: headers.retry_after_ms().unwrap_or(0),
        };
    }
    PandayError::Provider {
        upstream: "http".into(),
        message: format!("HTTP {status}: {body}"),
        // 5xx and 408 are worth another target; 4xx will fail identically.
        retryable: status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT,
    }
}

/// RFC 9110 `Retry-After`: delay-seconds, or IMF-fixdate.
fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs.saturating_mul(1000));
    }
    let fmt = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    let when = time::PrimitiveDateTime::parse(raw, &fmt).ok()?.assume_utc();
    let delta = when - time::OffsetDateTime::now_utc();
    if delta.is_negative() {
        Some(0)
    } else {
        u64::try_from(delta.whole_milliseconds().max(0)).ok()
    }
}

#[async_trait::async_trait]
impl HttpStreamTransport for ReqwestTransport {
    async fn post_sse(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<SseResponse, PandayError> {
        let mut req = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .body(body);

        for (name, value) in headers {
            req = req.header(name, value);
        }

        let resp = req.send().await.map_err(transport_error)?;

        let status = resp.status();
        // Copy headers before consuming the body — that is the whole of M25.3.
        let headers = ResponseHeaders::from_header_map(resp.headers());
        if !status.is_success() {
            // Read the body for the message: providers put the real reason
            // there, and losing it makes every failure look identical.
            let body = resp.text().await.unwrap_or_default();
            return Err(status_error(status, body, &headers));
        }

        // Mid-stream errors surface as stream items, not as a failed call:
        // docs/11 requires the gateway to emit `Error{retryable}` and let the
        // harness decide, because it holds turn semantics.
        let stream = futures_util::TryStreamExt::map_err(resp.bytes_stream(), transport_error);
        let stream = futures_util::StreamExt::map(stream, |r| r.map(|b| b.to_vec()));

        Ok(SseResponse {
            headers,
            body: Box::pin(stream),
        })
    }

    async fn get_json(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<u8>, PandayError> {
        let mut req = self.client.get(url).timeout(Duration::from_secs(8));
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let resp = req.send().await.map_err(transport_error)?;
        let status = resp.status();
        let headers = ResponseHeaders::from_header_map(resp.headers());
        let body = resp.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(status_error(
                status,
                String::from_utf8_lossy(&body).into_owned(),
                &headers,
            ));
        }
        Ok(body.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> ResponseHeaders {
        ResponseHeaders::default()
    }

    #[test]
    fn rate_limits_get_their_own_error_type() {
        let e = status_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            String::new(),
            &empty(),
        );
        assert!(matches!(e, PandayError::RateLimited { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn retry_after_seconds_fill_rate_limited_ms() {
        let headers = ResponseHeaders::from_pairs([("Retry-After", "5")]);
        let e = status_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            String::new(),
            &headers,
        );
        match e {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 5_000),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[test]
    fn missing_retry_after_stays_zero_rather_than_inventing_a_delay() {
        let e = status_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            String::new(),
            &empty(),
        );
        match e {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 0),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[test]
    fn a_past_http_date_retry_after_is_zero() {
        let headers =
            ResponseHeaders::from_pairs([("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT")]);
        assert_eq!(headers.retry_after_ms(), Some(0));
    }

    #[test]
    fn a_future_http_date_retry_after_is_positive() {
        let headers =
            ResponseHeaders::from_pairs([("retry-after", "Wed, 01 Jan 2098 00:00:00 GMT")]);
        let ms = headers.retry_after_ms().expect("parsed");
        assert!(ms > 0, "future Retry-After must not collapse to 0: {ms}");
    }

    #[test]
    fn server_errors_are_retryable_client_errors_are_not() {
        let e = status_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "boom".into(),
            &empty(),
        );
        assert!(e.is_retryable(), "5xx should move to the next target");

        let e = status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "bad model".into(),
            &empty(),
        );
        assert!(!e.is_retryable(), "replaying a 400 fails identically");
    }

    #[test]
    fn the_upstream_body_survives_into_the_error() {
        let e = status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "unknown model 'x'".into(),
            &empty(),
        );
        assert!(
            e.to_string().contains("unknown model 'x'"),
            "the real reason must not be swallowed: {e}"
        );
    }

    #[test]
    fn openai_remaining_headers_survive_on_the_success_path() {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert("X-RateLimit-Remaining-Requests", "42".parse().unwrap());
        map.insert("x-ratelimit-remaining-tokens", "8000".parse().unwrap());
        let headers = ResponseHeaders::from_header_map(&map);
        let remaining = headers.ratelimit_remaining();
        assert_eq!(remaining.requests, Some(42));
        assert_eq!(remaining.tokens, Some(8_000));
    }

    #[test]
    fn headroom_needs_a_limit_as_well_as_a_remainder() {
        // "412 requests left" is not a percentage until you know the ceiling.
        let only_remaining =
            ResponseHeaders::from_pairs([("x-ratelimit-remaining-requests", "412")]);
        assert_eq!(only_remaining.ratelimit_remaining().headroom_pct(), None);
        assert!(only_remaining.ratelimit_remaining().is_present());

        let both = ResponseHeaders::from_pairs([
            ("x-ratelimit-remaining-requests", "250"),
            ("x-ratelimit-limit-requests", "1000"),
        ]);
        assert_eq!(both.ratelimit_remaining().headroom_pct(), Some(0.25));
    }

    #[test]
    fn headroom_reports_the_scarcer_of_requests_and_tokens() {
        // 90% of requests but 3% of tokens is 3% of headroom. Reporting the
        // kinder number would hide the one that is about to bite.
        let h = ResponseHeaders::from_pairs([
            ("anthropic-ratelimit-requests-remaining", "90"),
            ("anthropic-ratelimit-requests-limit", "100"),
            ("anthropic-ratelimit-tokens-remaining", "3000"),
            ("anthropic-ratelimit-tokens-limit", "100000"),
        ]);
        assert_eq!(h.ratelimit_remaining().headroom_pct(), Some(0.03));
    }

    #[test]
    fn no_ratelimit_headers_at_all_is_absent_not_zero() {
        // SuperGrok, Claude Max and Codex OAuth send none of these. Absent must
        // not read as "no headroom left" — the local counters stay in charge.
        let h = ResponseHeaders::default().ratelimit_remaining();
        assert!(!h.is_present());
        assert_eq!(h.headroom_pct(), None);
    }

    #[test]
    fn a_zero_limit_does_not_divide_by_zero() {
        let h = ResponseHeaders::from_pairs([
            ("x-ratelimit-remaining-tokens", "0"),
            ("x-ratelimit-limit-tokens", "0"),
        ]);
        assert_eq!(h.ratelimit_remaining().headroom_pct(), None);
    }

    #[test]
    fn anthropic_remaining_headers_survive_on_the_success_path() {
        let headers = ResponseHeaders::from_pairs([
            ("anthropic-ratelimit-requests-remaining", "3"),
            ("anthropic-ratelimit-tokens-remaining", "12000"),
        ]);
        let remaining = headers.ratelimit_remaining();
        assert_eq!(remaining.requests, Some(3));
        assert_eq!(remaining.tokens, Some(12_000));
    }
}
