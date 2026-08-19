//! The `cheap`-pool summarizer behind docs/15's layer 5 (M15.6).
//!
//! docs/15: "a `cheap`-pool model (later: our own tuned 2-4B summarizer — Model 2
//! in 19.5) digests what structure can't."
//!
//! It lives in the harness rather than the reducer because it needs a
//! `ModelClient`, and `panday-reducer` depends on `panday-types` and nothing else
//! — which is what lets the reducer be tested without a model at all.
//!
//! ## The swap-in point
//!
//! `Summarizer` is the seam M19.5's tuned model drops into: same trait, different
//! `name()` and `cost_micros()`. Nothing above it changes, and the reduce-then-solve
//! eval (M15.5) is what decides whether the swap was an improvement — the scorecard
//! compares task success, so a cheaper summarizer that loses facts fails the gate
//! rather than looking like a win.

use panday_reducer::{SummarizeError, Summarizer};
use panday_sdk::{ModelClient, PandayError};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StreamItem, TaskClass,
};
use panday_types::{AccountId, RequestId, SessionId, TurnId};
use std::sync::Arc;

/// Summarizes through the gateway, letting the router pick the pool.
pub struct CheapPoolSummarizer {
    client: Arc<dyn ModelClient>,
    /// `auto` by default: docs/12's policy sends `task: summarize` under 8k
    /// context to the cheap pool, and hard-coding a model here would bypass the
    /// one component whose job is choosing one.
    model: ModelRef,
    pricing: panday_reducer::Pricing,
    account: AccountId,
    session: SessionId,
    label: String,
}

impl CheapPoolSummarizer {
    pub fn new(client: Arc<dyn ModelClient>, account: AccountId, session: SessionId) -> Self {
        Self {
            client,
            model: ModelRef::auto(),
            // The cheap pool's shape, not a specific model's — the router may pick
            // any member. Under-estimating would let the tier run when it should
            // not, so `openai_compat_class` (the more expensive of the cheap
            // options) is the honest default.
            pricing: panday_reducer::Pricing::openai_compat_class(),
            account,
            session,
            label: "cheap-pool".into(),
        }
    }

    /// Pin a model and its price — used when a caller knows better than the
    /// policy, and by M19.5 to point at the tuned summarizer.
    pub fn with_model(mut self, model: ModelRef, pricing: panday_reducer::Pricing) -> Self {
        self.label = model.0.clone();
        self.model = model;
        self.pricing = pricing;
        self
    }
}

/// The prompt. Terse on purpose: every token here is paid for on every
/// summarization, and a long preamble is a fixed cost on a variable saving.
fn prompt(text: &str, target_tokens: u32) -> String {
    format!(
        "Compress the tool output below to at most {} words. Keep every concrete \
         fact an engineer would need to act: file paths with line numbers, error \
         codes, failing test names, counts, exact identifiers. Drop prose, \
         progress lines and repetition. Do not add commentary.\n\n---\n{text}",
        // ~0.75 words per token is the usual English ratio; asking in words
        // because a model cannot count its own tokens.
        (target_tokens as f32 * 0.75) as u32
    )
}

#[async_trait::async_trait]
impl Summarizer for CheapPoolSummarizer {
    fn name(&self) -> &str {
        &self.label
    }

    fn cost_micros(&self, input_tokens: u32, target_tokens: u32) -> u64 {
        // Input plus the generated summary, at this pool's price. The prompt
        // preamble is ~60 tokens and is counted in `input_tokens` by the caller's
        // measure of the text, so this rounds slightly low — which biases toward
        // running the tier, so `SemanticConfig::min_value_micros` carries the
        // margin rather than a fudge factor here.
        let input = self
            .pricing
            .input_per_mtok_micros
            .saturating_mul(input_tokens as u64)
            / 1_000_000;
        let output = self
            .pricing
            .output_per_mtok_micros
            .saturating_mul(target_tokens as u64)
            / 1_000_000;
        input + output
    }

    async fn summarize(&self, text: &str, target_tokens: u32) -> Result<String, SummarizeError> {
        use futures_util::StreamExt;

        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: prompt(text, target_tokens),
                }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling {
                // Deterministic: a summary that varies between runs makes the
                // reduce-then-solve eval unrepeatable, and there is nothing
                // creative being asked for.
                temperature: Some(0.0),
                max_tokens: Some(target_tokens),
                ..Default::default()
            },
            cache: Default::default(),
            stream: true,
            metadata: CallMeta {
                account: self.account,
                request: RequestId::new(),
                // Attributed to the session it serves, so its cost lands on the
                // right ledger row rather than looking like unattributed traffic.
                session: Some(self.session),
                turn: Some(TurnId::new()),
                // The declared class is what routes this to the cheap pool.
                task: Some(TaskClass::Summarize),
            },
        };

        let mut stream = self.client.chat(req).await.map_err(as_error)?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item.map_err(as_error)? {
                StreamItem::Delta { text: t } => text.push_str(&t),
                StreamItem::Done { .. } => break,
                _ => {}
            }
        }
        if text.trim().is_empty() {
            // An empty summary would look like a 100% reduction, which is the most
            // expensive possible bug in this layer.
            return Err(SummarizeError::Unavailable("empty summary".into()));
        }
        Ok(text)
    }
}

fn as_error(e: PandayError) -> SummarizeError {
    // Timeout is not a variant of `PandayError` (M10.2 puts the deadline in the
    // `Timeout` middleware, which surfaces as a provider error), so anything that
    // is not clearly a timeout is reported as unavailable rather than guessed at.
    match &e {
        PandayError::Provider { message, .. } if message.to_lowercase().contains("timeout") => {
            SummarizeError::Timeout(0)
        }
        _ => SummarizeError::Unavailable(e.to_string()),
    }
}
