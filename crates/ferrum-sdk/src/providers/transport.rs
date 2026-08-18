//! The HTTP seam.
//!
//! The dialect (`wire`) and the stream machine (`ChunkTranslator`) are pure;
//! this is the only place that knows a socket exists. Isolating it is what
//! lets the whole adapter be tested without a model running — tests supply a
//! `MockTransport` and the suite never opens a connection.

use crate::FerrumError;
use futures_core::Stream;
use std::pin::Pin;

/// A stream of raw response-body chunks. Chunk boundaries are arbitrary and
/// may split a line or a UTF-8 sequence; `SseDecoder` handles that.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, FerrumError>> + Send>>;

/// Issues one streaming POST and hands back the body.
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
    ) -> Result<ByteStream, FerrumError>;
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
fn transport_error(e: reqwest::Error) -> FerrumError {
    let retryable = e.is_timeout() || e.is_connect() || e.is_request();
    FerrumError::Provider {
        upstream: "http".into(),
        message: e.to_string(),
        retryable,
    }
}

/// Map a non-2xx response to the IR error vocabulary.
fn status_error(status: reqwest::StatusCode, body: String) -> FerrumError {
    // 429 carries its own type so the gateway's failover can back off rather
    // than burn the next provider in the chain.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return FerrumError::RateLimited { retry_after_ms: 0 };
    }
    FerrumError::Provider {
        upstream: "http".into(),
        message: format!("HTTP {status}: {body}"),
        // 5xx and 408 are worth another target; 4xx will fail identically.
        retryable: status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT,
    }
}

#[async_trait::async_trait]
impl HttpStreamTransport for ReqwestTransport {
    async fn post_sse(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<ByteStream, FerrumError> {
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
        if !status.is_success() {
            // Read the body for the message: providers put the real reason
            // there, and losing it makes every failure look identical.
            let body = resp.text().await.unwrap_or_default();
            return Err(status_error(status, body));
        }

        // Mid-stream errors surface as stream items, not as a failed call:
        // docs/11 requires the gateway to emit `Error{retryable}` and let the
        // harness decide, because it holds turn semantics.
        let stream = futures_util::TryStreamExt::map_err(resp.bytes_stream(), transport_error);
        let stream = futures_util::StreamExt::map(stream, |r| r.map(|b| b.to_vec()));

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_get_their_own_error_type() {
        let e = status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, String::new());
        assert!(matches!(e, FerrumError::RateLimited { .. }));
        assert!(e.is_retryable());
    }

    #[test]
    fn server_errors_are_retryable_client_errors_are_not() {
        let e = status_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "boom".into());
        assert!(e.is_retryable(), "5xx should move to the next target");

        let e = status_error(reqwest::StatusCode::BAD_REQUEST, "bad model".into());
        assert!(!e.is_retryable(), "replaying a 400 fails identically");
    }

    #[test]
    fn the_upstream_body_survives_into_the_error() {
        let e = status_error(reqwest::StatusCode::BAD_REQUEST, "unknown model 'x'".into());
        assert!(
            e.to_string().contains("unknown model 'x'"),
            "the real reason must not be swallowed: {e}"
        );
    }
}
