//! Credential inner-loop for one provider name (docs/25).
//!
//! The registry still sees one adapter (`"xai"`, `"anthropic"`, …). Members are
//! API keys or OAuth tokens. `RateLimited` / retryable errors walk to the next
//! key; a non-retryable 400 does not. Every member 429s → `RateLimited` so the
//! model chain can still fail over to another provider.
//!
//! [`Rotate::Failover`] always starts at the first key. [`Rotate::RoundRobin`]
//! spreads new requests, then still walks on 429.

use super::anthropic::Anthropic;
use super::openai_compat::OpenAiCompat;
use crate::circuit::CredentialBreakers;
use crate::{AdapterCaps, ProviderAdapter};
use panday_sdk::providers::RemoteModel;
use panday_sdk::{ItemStream, PandayError};
use panday_types::id::SessionId;
use panday_types::model::ChatRequest;
use std::sync::{Arc, Mutex};

/// How the next request picks its first credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotate {
    /// Always try member 0, then 1, … on 429. Predictable; sticky-ish.
    Failover,
    /// Each new request starts one member further on. Still walks on 429.
    RoundRobin,
}

impl Rotate {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "failover" | "fail-over" | "ordered" => Some(Rotate::Failover),
            "round_robin" | "round-robin" | "rr" => Some(Rotate::RoundRobin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Rotate::Failover => "failover",
            Rotate::RoundRobin => "round_robin",
        }
    }
}

/// Public row. Never contains the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberMeta {
    pub id: String,
    pub provider: String,
    pub kind: String,
    pub label: String,
    pub last4: String,
}

struct Member {
    meta: MemberMeta,
    adapter: Arc<dyn ProviderAdapter>,
}

/// One credential to try: its stable id, and the adapter holding its secret.
/// The id travels with the adapter because every outcome is recorded against
/// that credential's own breaker (docs/25 M25.5).
type PoolMember = (String, Arc<dyn ProviderAdapter>);

/// Several credentials behind one registry name. Interior-mutable so the
/// operator console can add/revoke without restarting the process.
pub struct PooledAdapter {
    dialect_name: &'static str,
    provider: String,
    rotate: Mutex<Rotate>,
    cursor: Mutex<usize>,
    members: Mutex<Vec<Member>>,
    /// One breaker per credential (docs/25 M25.5). Without this, a single dead
    /// key's failures accumulate against the `(provider, model)` breaker and
    /// eventually open the route for every healthy sibling too.
    breakers: CredentialBreakers,
}

impl PooledAdapter {
    pub fn empty(provider: impl Into<String>, dialect_name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            dialect_name,
            provider: provider.into(),
            rotate: Mutex::new(Rotate::Failover),
            cursor: Mutex::new(0),
            members: Mutex::new(Vec::new()),
            breakers: CredentialBreakers::default(),
        })
    }

    pub fn new(members: Vec<Arc<dyn ProviderAdapter>>) -> Self {
        let dialect_name = members.first().map(|m| m.name()).unwrap_or("openai_compat");
        Self {
            dialect_name,
            provider: String::new(),
            rotate: Mutex::new(Rotate::Failover),
            cursor: Mutex::new(0),
            members: Mutex::new(
                members
                    .into_iter()
                    .map(|adapter| Member {
                        meta: MemberMeta {
                            id: uuid::Uuid::new_v4().to_string(),
                            provider: String::new(),
                            kind: "api_key".into(),
                            label: String::new(),
                            last4: String::new(),
                        },
                        adapter,
                    })
                    .collect(),
            ),
            breakers: CredentialBreakers::default(),
        }
    }

    pub fn set_rotate(&self, policy: Rotate) {
        *self.rotate.lock().expect("pool rotate") = policy;
    }

    pub fn rotate(&self) -> Rotate {
        *self.rotate.lock().expect("pool rotate")
    }

    pub fn is_empty(&self) -> bool {
        self.members.lock().expect("pool members").is_empty()
    }

    pub fn list(&self) -> Vec<MemberMeta> {
        self.members
            .lock()
            .expect("pool members")
            .iter()
            .map(|m| m.meta.clone())
            .collect()
    }

    pub fn push(
        &self,
        kind: &str,
        label: &str,
        last4: &str,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> MemberMeta {
        let meta = MemberMeta {
            id: uuid::Uuid::new_v4().to_string(),
            provider: self.provider.clone(),
            kind: kind.to_string(),
            label: if label.trim().is_empty() {
                format!("{}-{last4}", self.provider)
            } else {
                label.trim().to_string()
            },
            last4: last4.to_string(),
        };
        let out = meta.clone();
        self.members
            .lock()
            .expect("pool members")
            .push(Member { meta, adapter });
        out
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut members = self.members.lock().expect("pool members");
        let before = members.len();
        members.retain(|m| m.meta.id != id);
        // Ids are per-member and freshly generated, but clearing the history
        // keeps a revoked credential from leaving a tripped breaker behind for
        // an id that no longer exists (docs/25 M25.5).
        self.breakers.reset(id);
        before != members.len()
    }

    /// The members to try, in order, plus where to start.
    ///
    /// Ids come back with the adapters because the caller records each outcome
    /// against that credential's breaker (docs/25 M25.5).
    fn snapshot(&self, session: Option<SessionId>) -> (Rotate, usize, Vec<PoolMember>) {
        let rotate = *self.rotate.lock().expect("pool rotate");
        let members = self.members.lock().expect("pool members");
        let n = members.len();
        let start = if n == 0 {
            0
        } else {
            match (rotate, session) {
                // Failover means "always start at member 0"; a session does not
                // change that, and under Failover the pool is already sticky.
                (Rotate::Failover, _) => 0,
                // Sticky per session (M25.5). Round-robin exists to spread load,
                // but spreading it *within* one conversation makes every turn
                // land on a different account — a cold prompt cache each time,
                // and usage smeared across subscriptions for no gain. Hashing
                // the session spreads by conversation instead of by request.
                //
                // From the UUID bytes, not `DefaultHasher`: that hasher's output
                // is explicitly not stable across releases, and a sticky choice
                // that silently moves on a toolchain bump is not sticky.
                (Rotate::RoundRobin, Some(id)) => (id.0.as_u128() % n as u128) as usize,
                (Rotate::RoundRobin, None) => {
                    let mut c = self.cursor.lock().expect("pool cursor");
                    let i = *c % n;
                    *c = c.wrapping_add(1);
                    i
                }
            }
        };
        let adapters = members
            .iter()
            .map(|m| (m.meta.id.clone(), m.adapter.clone()))
            .collect();
        (rotate, start, adapters)
    }

    /// Breaker state for one credential, for the console and for tests.
    pub fn credential_state(&self, id: &str) -> crate::circuit::State {
        self.breakers.state(id)
    }
}

/// Last four Unicode scalars of a secret, for display. Never the secret.
pub fn last4(secret: &str) -> String {
    let n = secret.chars().count();
    secret.chars().skip(n.saturating_sub(4)).collect()
}

/// Split a pasted or env list of keys. Comma, semicolon, newline. Not colon
/// (`sk-…` keys are colon-free; paths use colon in `PANDAY_GROK_AUTH`).
pub fn split_keys(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn env_keys(primary: &str, extra: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(v) = std::env::var(primary) {
        out.extend(split_keys(&v));
    }
    if let Ok(v) = std::env::var(extra) {
        out.extend(split_keys(&v));
    }
    out
}

pub fn xai_oauth_adapter(token: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(OpenAiCompat::new(
        panday_sdk::oauth::xai_api_base(),
        Some(token.into()),
    )) as _
}

pub fn xai_key_adapter(key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    xai_oauth_adapter(key)
}

pub fn openai_key_adapter(base: &str, key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(OpenAiCompat::new(base, Some(key.into()))) as _
}

pub fn anthropic_key_adapter(key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(Anthropic::new(key)) as _
}

pub fn anthropic_oauth_adapter(token: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(Anthropic::oauth(token)) as _
}

/// 0 tokens → none; 1+ → a pool (one member is still a pool so the console can add more).
pub fn from_xai_tokens(tokens: Vec<String>) -> Option<Arc<PooledAdapter>> {
    let tokens: Vec<String> = tokens
        .into_iter()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let pool = PooledAdapter::empty("xai", "openai_compat");
    for (i, t) in tokens.into_iter().enumerate() {
        let label = format!("grok-{}", i + 1);
        let tail = last4(&t);
        pool.push("oauth", &label, &tail, xai_oauth_adapter(t));
    }
    Some(pool)
}

#[async_trait::async_trait]
impl ProviderAdapter for PooledAdapter {
    fn name(&self) -> &'static str {
        self.dialect_name
    }

    fn capabilities(&self, model: &str) -> AdapterCaps {
        let members = self.members.lock().expect("pool members");
        match members.first() {
            Some(m) => m.adapter.capabilities(model),
            None => AdapterCaps::default(),
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let (_rotate, start, members) = self.snapshot(req.metadata.session);
        if members.is_empty() {
            return Err(PandayError::Provider {
                upstream: if self.provider.is_empty() {
                    self.dialect_name.to_string()
                } else {
                    self.provider.clone()
                },
                message: "no credentials in this pool".into(),
                retryable: true,
            });
        }
        let n = members.len();
        let mut last_retryable: Option<PandayError> = None;
        let mut all_rate_limited = true;
        let mut soonest_retry_after_ms: Option<u64> = None;
        let mut tried = 0usize;
        for i in 0..n {
            let (id, member) = &members[(start + i) % n];
            // A credential whose breaker is open is skipped without being
            // called: that is the whole point of the breaker, and trying it
            // anyway would spend the caller's latency proving what we know.
            if !self.breakers.allow(id) {
                continue;
            }
            tried += 1;
            match member.chat(req.clone()).await {
                Ok(stream) => {
                    self.breakers.record_success(id);
                    return Ok(stream);
                }
                Err(e) if e.is_retryable() => {
                    self.breakers.record_failure(id);
                    if let PandayError::RateLimited {
                        retry_after_ms: wait,
                    } = &e
                    {
                        // 0 means the upstream sent no `Retry-After`, not "retry
                        // now" (docs/25 M25.3). A member that did not say must
                        // not out-vote one that did, so unknowns abstain rather
                        // than collapsing the minimum to zero.
                        if *wait > 0 {
                            soonest_retry_after_ms =
                                Some(soonest_retry_after_ms.map_or(*wait, |s| s.min(*wait)));
                        }
                    } else {
                        all_rate_limited = false;
                    }
                    last_retryable = Some(e);
                }
                // A 400 is the request's fault, not the credential's. Counting
                // it would let one malformed caller open every key in the pool.
                Err(e) => return Err(e),
            }
        }
        if tried == 0 {
            // Every credential is breaker-open. Retryable, so the model chain
            // walks to another provider rather than reporting this as the
            // caller's problem.
            return Err(PandayError::Provider {
                upstream: if self.provider.is_empty() {
                    self.dialect_name.to_string()
                } else {
                    self.provider.clone()
                },
                message: "every credential in this pool is circuit-open".into(),
                retryable: true,
            });
        }
        if all_rate_limited && last_retryable.is_some() {
            return Err(PandayError::RateLimited {
                retry_after_ms: soonest_retry_after_ms.unwrap_or(0),
            });
        }
        Err(last_retryable.unwrap_or_else(|| PandayError::Provider {
            upstream: self.dialect_name.into(),
            message: "pooled adapter has no members".into(),
            retryable: true,
        }))
    }

    async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        let adapters: Vec<Arc<dyn ProviderAdapter>> = {
            let members = self.members.lock().expect("pool members");
            members.iter().map(|m| m.adapter.clone()).collect()
        };
        if adapters.is_empty() {
            return Ok(Vec::new());
        }
        let mut last_retryable: Option<PandayError> = None;
        for adapter in adapters {
            match adapter.list_models().await {
                Ok(models) => return Ok(models),
                Err(e) if e.is_retryable() => last_retryable = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_retryable.unwrap_or_else(|| PandayError::Provider {
            upstream: self.dialect_name.into(),
            message: "pooled adapter has no members".into(),
            retryable: true,
        }))
    }
}

/// The four provider pools the console and boot share.
#[derive(Clone)]
pub struct CredHub {
    pub xai: Arc<PooledAdapter>,
    pub anthropic: Arc<PooledAdapter>,
    pub openai: Arc<PooledAdapter>,
    pub gemini: Arc<PooledAdapter>,
}

impl CredHub {
    pub fn new() -> Self {
        Self {
            xai: PooledAdapter::empty("xai", "openai_compat"),
            anthropic: PooledAdapter::empty("anthropic", "anthropic"),
            openai: PooledAdapter::empty("openai", "openai_compat"),
            gemini: PooledAdapter::empty("gemini", "openai_compat"),
        }
    }

    pub fn set_rotate(&self, policy: Rotate) {
        self.xai.set_rotate(policy);
        self.anthropic.set_rotate(policy);
        self.openai.set_rotate(policy);
        self.gemini.set_rotate(policy);
    }

    pub fn rotate(&self) -> Rotate {
        self.xai.rotate()
    }

    pub fn accounts(&self) -> Vec<MemberMeta> {
        let mut out = self.xai.list();
        out.extend(self.anthropic.list());
        out.extend(self.openai.list());
        out.extend(self.gemini.list());
        out
    }

    pub fn pool(&self, provider: &str) -> Option<Arc<PooledAdapter>> {
        match provider {
            "xai" => Some(self.xai.clone()),
            "anthropic" => Some(self.anthropic.clone()),
            "openai" => Some(self.openai.clone()),
            "gemini" => Some(self.gemini.clone()),
            _ => None,
        }
    }

    pub fn remove(&self, id: &str) -> bool {
        self.xai.remove(id)
            || self.anthropic.remove(id)
            || self.openai.remove(id)
            || self.gemini.remove(id)
    }

    pub fn contains_last4(&self, provider: &str, last4: &str) -> bool {
        self.accounts()
            .iter()
            .any(|a| a.provider == provider && a.last4 == last4)
    }
}

impl Default for CredHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheStyle;
    use futures_util::StreamExt;
    use panday_sdk::providers::transport::{HttpStreamTransport, SseResponse};
    use panday_types::id::{AccountId, RequestId};
    use panday_types::model::{
        CallMeta, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Spy {
        name: &'static str,
        calls: AtomicUsize,
        script: Script,
    }

    enum Script {
        RateLimited(u64),
        BadRequest,
        Retryable,
        Ok(&'static str),
    }

    impl Spy {
        fn new(name: &'static str, script: Script) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: AtomicUsize::new(0),
                script,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for Spy {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self, _model: &str) -> AdapterCaps {
            AdapterCaps {
                cache_style: CacheStyle::AutomaticPrefix,
                tools: true,
                ..Default::default()
            }
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.script {
                Script::RateLimited(retry_after_ms) => {
                    Err(PandayError::RateLimited { retry_after_ms })
                }
                Script::BadRequest => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 400: bad request".into(),
                    retryable: false,
                }),
                Script::Retryable => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 503: overloaded".into(),
                    retryable: true,
                }),
                Script::Ok(text) => {
                    let items: Vec<Result<StreamItem, PandayError>> = vec![
                        Ok(StreamItem::Delta { text: text.into() }),
                        Ok(StreamItem::Done {
                            reason: StopReason::EndTurn,
                        }),
                    ];
                    Ok(Box::pin(futures_util::stream::iter(items)))
                }
            }
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: ModelRef("xai/grok-4.6".into()),
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

    async fn drain_text(adapter: &dyn ProviderAdapter) -> Result<String, PandayError> {
        let mut stream = adapter.chat(req()).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: t } = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    fn pool(members: Vec<Arc<Spy>>) -> PooledAdapter {
        PooledAdapter::new(
            members
                .into_iter()
                .map(|m| m as Arc<dyn ProviderAdapter>)
                .collect(),
        )
    }

    /// A pool whose credential breakers trip fast, so a test does not need
    /// twenty calls to open one.
    fn pool_with_quick_breakers(members: Vec<Arc<Spy>>) -> PooledAdapter {
        let mut p = pool(members);
        p.breakers = crate::circuit::CredentialBreakers::new(crate::circuit::BreakerConfig {
            window: 4,
            min_samples: 2,
            error_rate: 0.5,
            cooldown: std::time::Duration::from_secs(30),
        });
        p
    }

    async fn drain_session(
        adapter: &dyn ProviderAdapter,
        session: SessionId,
    ) -> Result<String, PandayError> {
        let mut r = req();
        r.metadata.session = Some(session);
        let mut stream = adapter.chat(r).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: t } = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    #[tokio::test]
    async fn rate_limited_member_walks_to_the_next_key() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        assert_eq!(adapter.name(), "openai_compat");
        let text = drain_text(&adapter).await.expect("B serves the call");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn every_member_429_returns_rate_limited() {
        let a = Spy::new("openai_compat", Script::RateLimited(8_000));
        let b = Spy::new("openai_compat", Script::RateLimited(3_000));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => {
                assert_eq!(*retry_after_ms, 3_000, "the soonest Retry-After wins")
            }
            other => panic!("expected RateLimited, got {other}"),
        }
        assert!(err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    // ── M25.5: sticky sessions ───────────────────────────────────────────────

    #[tokio::test]
    async fn one_session_lands_on_the_same_credential_every_turn() {
        // Round-robin exists to spread load across accounts. Spreading it
        // *within* one conversation gives every turn a cold prompt cache and
        // smears one user's usage across subscriptions for no gain.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let c = Spy::new("openai_compat", Script::Ok("from-c"));
        let adapter = pool(vec![a.clone(), b.clone(), c.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);

        let session = SessionId::new();
        let first = drain_session(&adapter, session).await.unwrap();
        for _ in 0..6 {
            assert_eq!(
                drain_session(&adapter, session).await.unwrap(),
                first,
                "a session must not hop credentials between turns"
            );
        }
        // All seven turns went to one member.
        let calls = [a.calls(), b.calls(), c.calls()];
        assert!(
            calls.contains(&7) && calls.iter().filter(|c| **c == 0).count() == 2,
            "expected one member to take all 7 turns, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn different_sessions_still_spread_across_credentials() {
        // Sticky must not collapse into "everyone gets member 0" — that would
        // trade load-spreading away entirely rather than moving it from
        // per-request to per-conversation.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);

        // Enough distinct sessions that both members being used is overwhelming
        // (a fair hash misses only 1-in-2^39), and deterministic per session.
        for _ in 0..40 {
            drain_session(&adapter, SessionId::new()).await.unwrap();
        }
        assert!(
            a.calls() > 0 && b.calls() > 0,
            "sessions should distribute: a={} b={}",
            a.calls(),
            b.calls()
        );
    }

    #[tokio::test]
    async fn failover_ignores_the_session_because_it_is_already_sticky() {
        // Failover's contract is "always start at member 0". A session must not
        // quietly redefine that.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        for _ in 0..5 {
            assert_eq!(
                drain_session(&adapter, SessionId::new()).await.unwrap(),
                "from-a"
            );
        }
        assert_eq!(b.calls(), 0);
    }

    #[tokio::test]
    async fn a_sticky_session_still_walks_when_its_credential_is_rate_limited() {
        // Sticky is a preference, not a pin: a 429 must still fail over.
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(
            drain_session(&adapter, SessionId::new()).await.unwrap(),
            "from-b"
        );
    }

    // ── M25.5: per-credential breakers ───────────────────────────────────────

    #[tokio::test]
    async fn a_failing_credential_is_dropped_without_being_called_again() {
        let bad = Spy::new("openai_compat", Script::Retryable);
        let good = Spy::new("openai_compat", Script::Ok("from-good"));
        let adapter = pool_with_quick_breakers(vec![bad.clone(), good.clone()]);

        for _ in 0..4 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        let after_open = bad.calls();
        for _ in 0..4 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        assert_eq!(
            bad.calls(),
            after_open,
            "an open credential must stop being dialled, not just fail faster"
        );
        assert_eq!(
            adapter.credential_state(&adapter.list()[0].id),
            crate::circuit::State::Open
        );
    }

    #[tokio::test]
    async fn one_dead_credential_does_not_open_its_healthy_siblings() {
        // The reason per-credential breakers exist: with only the
        // (provider, model) breaker, a dead key's failures accumulate against
        // the whole route and eventually take the healthy keys down with it.
        let bad = Spy::new("openai_compat", Script::Retryable);
        let good = Spy::new("openai_compat", Script::Ok("from-good"));
        let adapter = pool_with_quick_breakers(vec![bad.clone(), good.clone()]);

        for _ in 0..8 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        assert_eq!(
            adapter.credential_state(&ids[0]),
            crate::circuit::State::Open
        );
        assert_eq!(
            adapter.credential_state(&ids[1]),
            crate::circuit::State::Closed,
            "the healthy sibling must stay closed"
        );
    }

    #[tokio::test]
    async fn a_pool_with_every_credential_open_is_retryable_so_the_chain_walks() {
        let a = Spy::new("openai_compat", Script::Retryable);
        let b = Spy::new("openai_compat", Script::Retryable);
        let adapter = pool_with_quick_breakers(vec![a.clone(), b.clone()]);

        for _ in 0..4 {
            let _ = drain_text(&adapter).await;
        }
        let calls_before = a.calls() + b.calls();
        let err = drain_text(&adapter).await.expect_err("all open");
        assert!(
            err.is_retryable(),
            "must stay retryable so the model chain fails over: {err}"
        );
        assert_eq!(
            a.calls() + b.calls(),
            calls_before,
            "nothing should be dialled once every credential is open"
        );
    }

    #[tokio::test]
    async fn a_bad_request_does_not_count_against_the_credential() {
        // A 400 is the caller's fault. Counting it would let one malformed
        // client open every key in the pool for everybody else.
        let a = Spy::new("openai_compat", Script::BadRequest);
        let adapter = pool_with_quick_breakers(vec![a.clone()]);
        for _ in 0..6 {
            let _ = drain_text(&adapter).await;
        }
        assert_eq!(
            adapter.credential_state(&adapter.list()[0].id),
            crate::circuit::State::Closed
        );
        assert_eq!(a.calls(), 6, "every attempt should still have been made");
    }

    #[tokio::test]
    async fn a_member_that_sent_no_retry_after_does_not_erase_one_that_did() {
        // 0 is "the upstream sent no header", not a zero-millisecond wait.
        // Folding it into the minimum would throw away the only real signal.
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(60_000));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                *retry_after_ms, 60_000,
                "the known wait survives an unknown sibling"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_pool_where_nobody_stated_a_wait_reports_zero() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(0));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                *retry_after_ms, 0,
                "no header anywhere means no wait to report, not an invented one"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_400_does_not_walk_keys() {
        let a = Spy::new("openai_compat", Script::BadRequest);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("caller error");
        assert!(!err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 0, "a 400 must not spend the next credential");
    }

    #[tokio::test]
    async fn retryable_provider_error_walks_like_429() {
        let a = Spy::new("openai_compat", Script::Retryable);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let text = drain_text(&adapter).await.expect("B serves");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn from_xai_tokens_matches_boot_shape() {
        assert!(from_xai_tokens(vec![]).is_none());
        assert!(from_xai_tokens(vec![String::new()]).is_none());
        let one = from_xai_tokens(vec!["sk-test-aaaa".into()]).expect("one token");
        assert_eq!(one.name(), "openai_compat");
        let two = from_xai_tokens(vec!["sk-test-aaaa".into(), "sk-test-bbbb".into()])
            .expect("two tokens");
        assert_eq!(two.name(), "openai_compat");
    }

    /// Minimal scripted HTTP: fail with an error, or replay one SSE body.
    struct MockTransport {
        fail: Mutex<Option<fn() -> PandayError>>,
        body: Vec<u8>,
        calls: AtomicUsize,
    }

    impl MockTransport {
        fn failing(err: fn() -> PandayError) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(Some(err)),
                body: Vec::new(),
                calls: AtomicUsize::new(0),
            })
        }
        fn streaming(body: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(None),
                body: body.to_vec(),
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl HttpStreamTransport for MockTransport {
        async fn post_sse(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: Vec<u8>,
        ) -> Result<SseResponse, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = *self.fail.lock().unwrap() {
                return Err(err());
            }
            Ok(SseResponse {
                headers: Default::default(),
                body: Box::pin(futures_util::stream::iter(vec![Ok(self.body.clone())])),
            })
        }
    }

    const FROM_B: &[u8] =
        br#"data: {"choices":[{"index":0,"delta":{"content":"from-b"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;

    #[tokio::test]
    async fn mock_transport_429_then_sse_from_the_second_key() {
        let a_http = MockTransport::failing(|| PandayError::RateLimited { retry_after_ms: 0 });
        let b_http = MockTransport::streaming(FROM_B);
        let a = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-aaaa".into()),
            a_http.clone(),
        );
        let b = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-bbbb".into()),
            b_http.clone(),
        );
        let adapter = PooledAdapter::new(vec![Arc::new(a) as _, Arc::new(b) as _]);
        let text = drain_text(&adapter).await.expect("B's SSE");
        assert_eq!(text, "from-b");
        assert_eq!(a_http.calls(), 1);
        assert_eq!(b_http.calls(), 1);
    }

    #[test]
    fn split_keys_accepts_comma_semicolon_and_newlines() {
        assert_eq!(
            split_keys("sk-test-aaaa, sk-test-bbbb"),
            vec!["sk-test-aaaa", "sk-test-bbbb"]
        );
        assert_eq!(
            split_keys("sk-test-aaaa;\nsk-test-bbbb\n"),
            vec!["sk-test-aaaa", "sk-test-bbbb"]
        );
        assert!(split_keys("  \n , ; ").is_empty());
        assert_eq!(last4("sk-test-aaaa"), "aaaa");
    }

    #[tokio::test]
    async fn empty_pool_is_retryable_so_the_model_chain_can_walk() {
        let pool = PooledAdapter::empty("xai", "openai_compat");
        let err = drain_text(pool.as_ref()).await.expect_err("empty");
        assert!(err.is_retryable(), "{err}");
        assert!(pool.list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn round_robin_starts_on_the_next_member() {
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-a");
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-a");
        assert_eq!(a.calls(), 2);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn round_robin_still_walks_on_429() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn three_api_keys_failover_skips_two_429s() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(0));
        let c = Spy::new("openai_compat", Script::Ok("from-c"));
        let adapter = pool(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-c");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
        assert_eq!(c.calls(), 1);
    }

    #[tokio::test]
    async fn live_add_and_remove_change_who_serves() {
        let pool = PooledAdapter::empty("openai", "openai_compat");
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        pool.push(
            "api_key",
            "paid",
            "aaaa",
            a.clone() as Arc<dyn ProviderAdapter>,
        );
        assert_eq!(drain_text(pool.as_ref()).await.unwrap(), "from-a");
        let id = pool.list()[0].id.clone();
        assert!(pool.remove(&id));
        assert!(pool.is_empty());
        let err = drain_text(pool.as_ref()).await.expect_err("removed");
        assert!(err.is_retryable());
    }

    #[test]
    fn rotate_parse() {
        assert_eq!(Rotate::parse("round-robin"), Some(Rotate::RoundRobin));
        assert_eq!(Rotate::parse("failover"), Some(Rotate::Failover));
        assert!(Rotate::parse("random").is_none());
    }
}
