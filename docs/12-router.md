# 12 — panday-router

Chooses which model serves each request. Lives inside the gateway process
(it's a library, not a service — one fewer hop). Policy first, learning
second (architecture sentence 4 in 01): a YAML file you can read beats a
model you can't debug, until the eval data says otherwise.

## The decision inputs

```rust
pub struct RouteQuery {
    pub requested: ModelRef,        // may be "auto" or a concrete pin
    pub task: TaskClass,            // from caller, or classified (see below)
    pub context_tokens: u32,
    pub needs: Caps,                // tools? vision? json-mode? min context?
    pub tenant: TenantRef,          // plan tier, privacy flags, region
    pub budget: BudgetState,        // remaining credits, soft/hard ceilings
    pub offline: bool,              // local daemon sets this
}
```

`TaskClass` v1 is deliberately coarse: `chat`, `code`, `summarize`,
`extract`, `route`, `embed`, `background`. Callers that know, say; otherwise
a heuristic classifier guesses (regex + length + tools-present rules to
start). The trained classifier (19) replaces the heuristic behind the same
trait — `Classifier: fn classify(&ChatRequest) -> (TaskClass, f32)` — and its
confidence gates whether we trust it.

## Policy file (v1)

```yaml
version: 1
pools:
  frontier:   [anthropic/claude-opus-*, openai/gpt-5*]
  workhorse:  [anthropic/claude-sonnet-*, together/qwen3.5-*-instruct]
  cheap:      [together/qwen3.5-9b, local/qwen3.5-4b]
  local-only: [local/qwen3.5-4b, local/gpt-oss-20b]

rules:
  - match: { offline: true }            # hard override, always first
    use: local-only
  - match: { task: route }              # never spend frontier tokens on routing
    use: cheap
  - match: { task: summarize, context_tokens: { lt: 8000 } }
    use: cheap
  - match: { task: code, plan: [pro, max] }
    use: frontier
    fallback: workhorse                  # budget pressure demotes here
  - match: {}                            # default
    use: workhorse

constraints:
  # tenant privacy flag → ONLY pools with zero external egress are admissible.
  # (Denying just `frontier` would still leak to workhorse/cheap providers.)
  privacy_strict: { restrict_to: [local-only] }
  budget_soft:    { demote_to: cheap }         # >80% spend: demote non-code tasks
```

Evaluation is first-match; the file is validated at load with unreachable-rule
detection. Every decision emits a `RouteDecision` audit record: query,
matched rule, chain, and — later — the counterfactual (what the learned
policy would have picked) so shadow evaluation is free.

## Learning, in stages (don't skip stages)

1. **Scorecards.** Eval suite (19) scores each pool per TaskClass weekly:
   quality, p50/p95 latency, $/task. Rendered into the policy file review —
   a human moves models between pools. This alone captures most of the value.
2. **Trained classifier.** ModernBERT-class encoder (~150M) fine-tuned on
   mined transcripts to predict TaskClass — replaces the heuristic, runs
   in-process via ONNX/candle in <5ms. First model we ever train
   (Model 1 in 19.5, shipped at M19.3).
3. **Learned cost/quality frontier.** Per (TaskClass, pool) success-rate from
   session outcomes (did the turn get retried? did tests pass?) feeds a
   bandit that *proposes* pool changes; a human approves. Full auto-routing
   is a phase-6 luxury, not a requirement.

## Offline mode

`offline: true` collapses the table to local pools and *also* returns the
active `CapabilityProfile` (max context, no vision, weaker JSON discipline)
which the harness injects into the system prompt — the model is told what it
is, so it stops promising what it can't do (18).

## Milestones

- **M12.1** Policy file parse + first-match engine + unreachable-rule linter; table-driven tests. ✅ *(shipped: `panday_router::policy` — `Policy`/`PolicyRouter`, the shipped default at `crates/panday-router/policy/default.yaml`, and `Policy::lint()`.)*

  Two notes from the implementation:

  - **Pool entries are globs, but `RouteDecision.chain` is `Vec<ModelRef>`.**
    Nothing yet resolves `anthropic/claude-opus-*` to a concrete model, so
    v1 chains carry the pattern through. Resolution needs the adapter
    registry (docs/11) and the local catalog (docs/18) — it lands with M12.2,
    when the router is wired into the gateway.
  - **`BudgetPressure::Hard` filters the chain to `local/` and can empty it**,
    yielding `NoRoute`. That is deliberate — a hit ceiling must never fall
    through to a paid call — but it means a policy whose pools contain no
    local model will hard-fail at the ceiling instead of degrading. The
    gateway turns that into a typed `budget_exceeded` and a graceful session
    pause (docs/11 §Quotas), rather than a 500.
- **M12.2** Wired into gateway: `auto` resolves through rules; RouteDecision audit rows land in PG.
- **M12.3** Heuristic classifier + confidence; misclassification harness with labeled fixtures.
- **M12.4** Scorecard generator from eval runs; the weekly review is a generated PR against the YAML.
- **M12.5** ONNX classifier slot behind `Classifier` trait; shadow-mode comparison report (heuristic vs learned) over 1k replayed sessions.
