//! M12.2 — the routing decision is recorded, including the ones that failed.
//!
//! docs/12 asks for "RouteDecision audit rows". The rows only earn their storage if they answer the
//! questions an operator actually asks: which rule sent this traffic, what chain did it produce,
//! what served it, and how many legs did it take to get there.

use futures_util::StreamExt;
use panday_gateway::{AdapterCaps, CollectRoutes, Gateway, ProviderAdapter};
use panday_router::{ModelCatalog, PolicyRouter};
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    TaskClass, Usage,
};
use std::sync::Arc;

const POLICY: &str = r#"
version: 1
pools:
  cheap: [local/qwen3.5-4b, together/qwen3.5-*-instruct]
rules:
  - match: {}
    use: cheap
constraints: {}
"#;

const CATALOG: &str = r#"
version: 1
models:
  - id: local/qwen3.5-4b
    context: 16000
  - id: together/qwen3.5-32b-instruct
    context: 128000
"#;

/// Answers, or fails in a way the chain walks past.
struct Leg {
    fail: Option<fn() -> PandayError>,
}

#[async_trait::async_trait]
impl ProviderAdapter for Leg {
    fn name(&self) -> &'static str {
        "leg"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        if let Some(f) = self.fail {
            return Err(f());
        }
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta { text: "hi".into() }),
            Ok(StreamItem::Usage {
                usage: Usage::default(),
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

fn leg(fail: Option<fn() -> PandayError>) -> Arc<dyn ProviderAdapter> {
    Arc::new(Leg { fail })
}

fn gateway(
    local: Arc<dyn ProviderAdapter>,
    together: Arc<dyn ProviderAdapter>,
    audit: Arc<CollectRoutes>,
) -> Gateway {
    let router = PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .with_catalog(ModelCatalog::from_yaml(CATALOG).unwrap());
    Gateway::builder(Arc::new(router))
        .adapter("local", local)
        .adapter("together", together)
        .route_audit(audit)
        .build()
}

fn req(model: &str, task: Option<TaskClass>) -> ChatRequest {
    ChatRequest {
        model: ModelRef(model.into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "say hi".into(),
            }],
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
            task,
        },
    }
}

async fn drain(g: &Gateway, r: ChatRequest) {
    let stream = g.chat(r).await.expect("call");
    let _: Vec<_> = stream.collect().await;
}

#[tokio::test]
async fn a_served_request_records_the_rule_the_chain_and_what_answered() {
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(leg(None), leg(None), audit.clone());
    let request = req("auto", Some(TaskClass::Chat));
    let (account, id) = (request.metadata.account, request.metadata.request);
    drain(&g, request).await;

    let rows = audit.take();
    assert_eq!(rows.len(), 1, "one row per request, not per attempt");
    let row = &rows[0];
    assert_eq!(row.account, account);
    assert_eq!(row.request, id);
    assert_eq!(row.requested, "auto");
    assert_eq!(row.matched_rule, "rules[0]");
    assert_eq!(row.pool, "cheap");
    // Concrete models, in failover order — the catalog did its job before the row was written.
    assert_eq!(
        row.chain,
        ["local/qwen3.5-4b", "together/qwen3.5-32b-instruct"]
    );
    assert_eq!(row.chosen.as_deref(), Some("local/qwen3.5-4b"));
    assert_eq!(row.attempts, 1);
}

#[tokio::test]
async fn a_failover_is_visible_as_two_attempts_and_the_model_that_actually_answered() {
    // The question this table exists for: "this rule looks fine, why is it slow?" — because its
    // first leg fails and every request pays for a dead provider before reaching the second.
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(
        leg(Some(|| PandayError::Provider {
            upstream: "together".into(),
            message: "overloaded".into(),
            retryable: true,
        })),
        leg(None),
        audit.clone(),
    );
    drain(&g, req("auto", None)).await;

    let rows = audit.take();
    assert_eq!(rows[0].attempts, 2);
    assert_eq!(
        rows[0].chosen.as_deref(),
        Some("together/qwen3.5-32b-instruct")
    );
}

#[tokio::test]
async fn an_exhausted_chain_is_recorded_with_no_model_chosen() {
    // The rows worth alerting on. A rule that only produces these is a rule pointing at something
    // broken, and it is invisible if failures are simply not written.
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(
        leg(Some(|| PandayError::Provider {
            upstream: "together".into(),
            message: "overloaded".into(),
            retryable: true,
        })),
        leg(Some(|| PandayError::Provider {
            upstream: "together".into(),
            message: "overloaded".into(),
            retryable: true,
        })),
        audit.clone(),
    );
    let err = g.chat(req("auto", None)).await.map(|_| ()).unwrap_err();
    assert!(matches!(err, PandayError::ModelUnavailable { .. }));

    let rows = audit.take();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].chosen, None);
    assert_eq!(rows[0].attempts, 2);
}

#[tokio::test]
async fn a_caller_error_is_recorded_too_and_does_not_walk_the_chain() {
    // A malformed request fails identically everywhere, so the gateway stops at the first leg. The
    // row still says which rule it was routed by — otherwise a client sending garbage looks like a
    // routing gap.
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(
        leg(Some(|| PandayError::Protocol("bad request".into()))),
        leg(None),
        audit.clone(),
    );
    let err = g.chat(req("auto", None)).await.map(|_| ()).unwrap_err();
    assert!(matches!(err, PandayError::Protocol(_)));

    let rows = audit.take();
    assert_eq!(rows[0].attempts, 1, "no chain walk on a caller error");
    assert_eq!(rows[0].chosen, None);
}

#[tokio::test]
async fn a_pin_is_recorded_as_what_the_caller_asked_for() {
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(leg(None), leg(None), audit.clone());
    drain(&g, req("local/qwen3.5-4b", None)).await;

    let rows = audit.take();
    assert_eq!(rows[0].requested, "local/qwen3.5-4b");
    assert_eq!(rows[0].matched_rule, "pinned");
    // `pinned`, not an empty string: an empty pool label on a dashboard reads as a bug rather than
    // as "the caller chose the model themselves".
    assert_eq!(rows[0].pool, "pinned");
}

#[tokio::test]
async fn the_recorded_class_is_the_one_the_router_keyed_on() {
    // The classifier's guess, when the caller declared nothing. Without this the audit cannot
    // explain why a rule matched, because the guess is not recoverable from the request afterwards.
    let audit = Arc::new(CollectRoutes::new());
    let g = gateway(leg(None), leg(None), audit.clone());
    drain(&g, req("auto", Some(TaskClass::Code))).await;
    assert_eq!(audit.take()[0].task, TaskClass::Code);
}

#[tokio::test]
async fn a_rule_this_deployment_cannot_serve_is_recorded_with_no_attempts() {
    // The signature of a misconfigured deployment rather than a failing provider: the router picked
    // a chain, and not one leg of it has an adapter here. Without a row, the only evidence is a 503
    // in a log — and the rule that produced it is invisible.
    let router = PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .with_catalog(ModelCatalog::from_yaml(CATALOG).unwrap());
    let audit = Arc::new(CollectRoutes::new());
    let g = Gateway::builder(Arc::new(router))
        // No adapters at all.
        .route_audit(audit.clone())
        .build();

    let err = g.chat(req("auto", None)).await.map(|_| ()).unwrap_err();
    assert!(matches!(err, PandayError::ModelUnavailable { .. }));

    let rows = audit.take();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempts, 0);
    assert_eq!(rows[0].chosen, None);
    assert_eq!(rows[0].matched_rule, "rules[0]");
    // The chain the *router* chose, not the callable subset — which is empty, and would tell an
    // operator nothing about which models the rule points at.
    assert_eq!(
        rows[0].chain,
        ["local/qwen3.5-4b", "together/qwen3.5-32b-instruct"]
    );
}
