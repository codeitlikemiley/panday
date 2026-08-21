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
  frontier:   [anthropic/claude-fable-*, anthropic/claude-opus-*, openai/gpt-5.6-sol]
  workhorse:  [anthropic/claude-sonnet-*, openai/gpt-5.6-terra, xai/grok-*, gemini/*, together/qwen3.5-*-instruct]
  cheap:      [openai/gpt-5.6-luna, together/qwen3.5-9b, local/qwen3.5-4b]
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

The type landed at M18.4 in `panday_types::capability`, and `panday local` applies it by default.
Since M12.2 the router returns it per route: profiles live in the model catalog, and
`RouteDecision.profile` carries the *head* of the chain's — the model the turn will actually run
against. A failover to a weaker leg is a different conversation, and one the harness is told about
when it happens rather than pre-emptively. `None` means no catalog is configured or the catalog does
not know the model, which reads as "no claim", not as "no capabilities".

## Milestones

- **M12.1** Policy file parse + first-match engine + unreachable-rule linter; table-driven tests. ✅ *(shipped: `panday_router::policy` — `Policy`/`PolicyRouter`, the shipped default at `crates/panday-router/policy/default.yaml`, and `Policy::lint()`.)*

  Two notes from the implementation:

  - **Pool entries are globs, but `RouteDecision.chain` is `Vec<ModelRef>`.**
    v1 chains carried the pattern through because nothing could resolve
    `anthropic/claude-opus-*` to a concrete model. **Closed at M12.2** by
    `panday_router::catalog`; a router with no catalog still passes patterns
    through, which is what keeps the offline tier working.
  - **`BudgetPressure::Hard` filters the chain to `local/` and can empty it**,
    yielding `NoRoute`. That is deliberate — a hit ceiling must never fall
    through to a paid call — but it means a policy whose pools contain no
    local model will hard-fail at the ceiling instead of degrading. The
    gateway turns that into a typed `budget_exceeded` and a graceful session
    pause (docs/11 §Quotas), rather than a 500.
- **M12.2** Wired into gateway: `auto` resolves through rules; RouteDecision audit rows land in PG. ✅ *(shipped: `panday_router::catalog` — `ModelCatalog`, the shipped file at `crates/panday-router/catalog/default.yaml`, `PolicyRouter::with_catalog`; `panday_gateway::RouteAudit`/`RouteRecord`; `panday_platform::routes` with migration `0005_route_decisions.sql`.)*

  **The catalog is what closes M12.1's note.** Pool entries are patterns so a policy outlives a
  model release; the catalog is the ordered list of models that exist, and expansion happens last —
  after every constraint — so privacy, demotion and the hard-budget filter all still reason in the
  vocabulary the policy file is written in. Expansion order is catalog order, which means editing
  the file reorders failover. That is the intended control, and it is why the list is not sorted.

  **Absent catalog ≠ empty catalog.** No catalog passes patterns through unchanged, exactly as
  before M12.2 — `panday local` routes to whatever model name the llama-server in front of it is
  serving, which is not a fact our file can know (ADR-011). An *empty* catalog is the other
  statement — "this deployment has no models" — and correctly yields `NoRoute`. Collapsing the two
  into one possibly-empty list would force the offline tier to maintain a catalog of models it
  cannot enumerate.

  **The chain is deduplicated.** A pool and its fallback routinely overlap (`cheap` and `local-only`
  share the 4B model). Left alone, failover would retry a model that just failed before moving on:
  a retry that does nothing, slowly.

  **Only hard capabilities filter.** A model that cannot hold the context or cannot see the image is
  dropped from the chain. `json_reliability` and `tool_reliability` are in the profile and are
  deliberately *not* admission criteria — they are soft numbers the harness adapts to (docs/18: the
  model is told what it is), and a router that filtered on them would make every local-only
  deployment unroutable the moment a request carried a tool. A glob that matches nothing stays
  empty (a pool with no models). An *exact* pin the catalog does not know is kept: live
  `/v1/models` lists names the YAML overlay has not priced yet (agy's `gemini-3.1-pro` is the
  example), and emptying the chain would 400 a request the provider would have served. A typo
  still fails at the provider.

  **The audit row is one per request, not one per attempt.** `attempts` and `chosen` carry the
  failover story: `attempts: 2, chosen: together/...` is the row that explains why a healthy-looking
  rule is slow, and `chosen: NULL` is the row worth alerting on. A row per leg would multiply the
  largest table in the system by the failure rate of the worst provider.

  **Written off the request path, and content-free.** `PgRouteAudit` hands the insert to a
  background task and warns on failure: an audit row is evidence, not money, and losing one to a
  database blip must degrade the dashboard rather than the request — the exact opposite of the
  ledger's `OnWriteFailure` contract, and the difference is the point. The row carries model ids, a
  rule name, a pool and counts; no prompt, no completion (docs/20 T5), which is what makes it safe
  to keep long enough to be useful. `routes::prune` exists because an audit table with no retention
  is a disk-full incident with a scheduled date.

  **Prices live in the catalog too.** One file answers "what models are there" and "what do they
  cost", because a model in a pool with no price is a call the meter cannot cost. A model with no
  `price` row is *unpriced* rather than free — `panday_unpriced_calls_total` counts it — while local
  models carry an explicit zero, because "this reduction saved no money" is a true and useful
  statement (ADR-007).
- **M12.3** Heuristic classifier + confidence; misclassification harness with labeled fixtures. ✅ *(shipped: `panday_router::classify` — `HeuristicClassifier`, `TRUST_THRESHOLD`, `classify_or_default`; harness in `crates/panday-router/tests/classification.rs`.)*

  **Measured: 94% (47/50), zero confidently-wrong.** The second number is the one that matters.
  Being wrong is tolerable; being wrong *and* trusted is not, because the router acts on it and
  nothing downstream can tell. The three it misses are genuinely ambiguous and are correctly
  reported below the trust gate, so they fall back rather than mislead.

  **The first number was 92% on 24 cases, and that was flattering.** Doubling the corpus at M19.1
  dropped the same classifier to 66% and produced two *confidently* wrong answers — a corpus that
  small had been measuring the cases somebody thought of while writing the classifier. What the
  bigger corpus found was three real gaps: routing questions ("which model should handle this")
  had no markers at all and fell through to `Code`; most summarise and extract asks do not contain
  the words "summarise" or "extract"; and single weak markers like bare `test` or `build` were
  trusted, so "the driving test is on tuesday" was confidently code. Fixing those took the score to
  94% on the harder corpus. The lesson is the general one about evals: a suite you pass is not
  evidence until it is a suite that could have failed.

  **Ambiguity is reported, not resolved.** When two marker families both fire —
  "tldr on why the cargo build broke" is honestly both summarize and code — the
  winner is returned *below* `TRUST_THRESHOLD`. Picking whichever matched one
  more keyword would be a guess dressed as a fact.

  **The corpus contains keyword traps, and they caught a real bug.** The first
  version matched substrings, so `"legit good"` matched `git `,
  `"class dismissed"` matched `class `, and `"can we fix a time"` matched
  `fix` — all classified as `Code`, confidently. Markers are now regexes with
  explicit word boundaries, and the genuinely ambiguous verbs (`fix`,
  `implement`, `class`) were removed rather than boundary-matched, because "fix
  a time" is ordinary English and no boundary saves it.

  An earlier draft of the corpus scored 100%, which is why the hard cases exist:
  a corpus the classifier aces measures nothing.
- **M12.4** Scorecard generator from eval runs; the weekly review is a generated PR against the YAML. ✅ *(shipped: `cargo xtask scorecard`, `panday_router::bench`, `.github/workflows/weekly-review.yml`.)*

  **The evals became libraries first.** route-bench's corpus and scoring moved out of
  `tests/classification.rs` into `panday_router::bench`, and reduce-bench's corpus into
  `panday_harness::eval::recorded_corpus` — because an eval that lives only inside a `#[test]`
  can be *checked* but never *reported*, and this milestone's deliverable is a report. The
  suites are now one caller each; the scorecard is another, and they cannot disagree about the
  numbers because there is one definition.

  It reads the evals directly rather than parsing `cargo test` output, which would break the
  first time someone renamed a test.

  **It does not edit the policy YAML, and that is the design.** Removing an unreachable rule
  changes where traffic goes, and a generator that rewrote routing on its own would be the one
  component in this repo making a routing decision nobody reviewed. The scorecard names what
  the linter found; the PR is where a human makes the edit. That reading of "a generated PR
  against the YAML" is stated here rather than buried in the workflow.

  **The PR carries the scorecard as a committed file**, so the numbers have a history: a report
  that only exists in a job log cannot be compared with last week's, and "is the router earning
  its keep" is a question about a trend. The job opens the PR even when a gate fails — a failing
  scorecard is the most important one to read — and the exit code still carries the gate for
  anything that wants to block on it.

  Current numbers: heuristic classifier **94%** (47/50), **zero** confidently wrong, three
  misses caught by the confidence gate; all three policy files lint clean. json-bench has a
  measured card for `xai/grok-4.6` (200/200, 2026-08-20); scoring an *agent* on agent-bench
  and capability profiles for the local catalog are still unmeasured. The weekly scorecard
  lists those gaps on purpose — a report that only shows what passed reads as coverage it
  does not have.
- **M12.5** ONNX classifier slot behind `Classifier` trait; shadow-mode comparison report (heuristic vs learned) over 1k replayed sessions. ✅ *(shipped as the harness: `panday_router::shadow`, `crates/panday-router/tests/shadow.rs`. **Scoped deliberately** — the ONNX runtime lands with M19.3.)*

  The `Classifier` trait was already the slot; a learned model implements it and
  nothing above changes. Adding a native inference dependency now would be weight
  without a payload — there is no trained model to load until M19.3 — so what shipped
  is the part that makes *using* the slot safe.

  **Shadow mode, not an A/B split.** A split sends real requests to the candidate, so
  its mistakes reach users and its wins are measured on different traffic than its
  losses. Shadow mode runs both on the *same* request and keeps the incumbent's
  answer: every disagreement is like-for-like and a bad candidate costs only CPU. The
  safety property is enforced by construction — `classify` returns the incumbent's
  answer — rather than by a flag, because "shadow mode, but live" is a configuration
  nobody should be able to express by accident.

  **The report is content-free.** Counts, classes and confidences, never prompt text
  (docs/21 T5): a "disagreement sample" holding the prompt would be the most quotable
  content leak in the platform. Samples carry a stable 16-hex digest of the classified
  text instead, which is joinable to the log by someone with access and comparable
  across runs — a random id would make this week's report incomparable with last
  week's.

  **Confident disagreements are counted separately.** A candidate that disagrees while
  being *more* confident is claiming to know better, which is what docs/12's
  confidence gate exists to catch.

  `ready_for_review` deliberately says nothing about accuracy: shadow mode measures
  *change*, and change is not improvement until someone scores it against labels
  (M12.3's misclassification harness). A test makes that explicit by shadowing a
  useless classifier with a copy of itself — perfect agreement, both wrong, and the
  report cannot tell.
