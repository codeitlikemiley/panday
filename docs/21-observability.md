# 21 — Observability

The platform is event-sourced; observability is mostly *projection*, not
instrumentation. Three pillars, one id scheme.

## The id scheme (everything joins on these)

`account_id / session_id / turn_id / seq / request_id / call_id` — UUIDv7,
propagated: AEP envelope → tracing span fields → gateway request → ledger
`source` → provider metadata where supported. One id in hand reaches
everything else.

## Traces

- `tracing` + OpenTelemetry OTLP export. Span tree per turn:
  `turn > assemble > model.call > tool.gate > sandbox.exec > reduce`.
- Spans carry counts and costs, **never content** by default (T5): token
  counts, cache splits, reduction ratios, decision enums.
- Collector: any OTLP backend (self-host Grafana Tempo/Jaeger at first).
  The *session replay* debugging story is the event log itself — traces are
  for latency and fan-out, the log is for "what happened".

## Metrics (the ones that run the business)

| Metric | Why it matters |
|---|---|
| cache-read ratio per session | ADR-008 compliance; the 1x-vs-6x input-cost lever |
| $ saved by reducer (est) per session | 15's dollar-denominated mandate |
| $ COGS per session / per turn, by pool | pricing reality; feeds 17 plan design |
| route decisions by rule + counterfactuals | is the router earning its keep |
| provider error/latency by (provider, model) | failover health; circuit state |
| sandbox-seconds by tier | the second metered good |
| ledger vs provider-invoice drift | must be ~0; alarm at 1% |
| turn stop-reasons distribution | budget stops climbing = UX problem brewing |
| upstream calls by (provider, outcome) | which subscription or key the traffic is actually landing on, and how much of it the provider refuses (docs/25 M25.6) |

Prometheus endpoint per service; Grafana dashboards checked into `deploy/`.

**Credentials are not a label.** `panday_upstream_calls_total` is labelled by
provider and outcome, never by credential id: an operator adds and revokes keys
freely, so a series per credential would make this metric's cardinality a
function of how often they do — the same unbounded-label mistake as the two rows
below. Per-credential counters live on the console's `GET /accounts` instead,
where a human reads them and retiring one costs nothing.

**Two rows of that table are not Prometheus-shaped, resolved at M21.2.**
"cache-read ratio *per session*" and "$ COGS *per session*" cannot be labels:
sessions are unbounded, and one series per session is how a monitoring system
falls over. The split is per-session numbers live in the ledger and the event
log — which is where a question about one session belongs, and what `panday
replay` answers — while Prometheus carries the *distribution* over sessions,
which is what a dashboard and an alert can use. Every label in the metric set is
bounded: provider, model, pool, rule, tier, stop reason, outcome, strategy. The
cap is also enforced at runtime, because a bug that puts an id in a label should
degrade the metric rather than the process.

## Logs

`tracing` JSON to stdout, shipped by the platform (Loki/Vector at first).
Content-free by default; `PANDAY_DEBUG_CONTENT=1` per-service for local dev
only (refuses to start with it set in `env=production`).

## The replay tool (the killer feature)

`panday replay <log>` — renders any session's event log as the CLI
would have shown it, with `--at seq` time travel, `--diff` between two
replays (e.g., before/after a reducer change), and `--costs` per-turn ledger
overlay. Built once in phase 2 against the fold; pays for itself the first
week. This tool is why state-must-fold-from-log is an invariant and not a
preference (ADR-002).

**Amended at M21.3: `<log>`, not `<session_id>`.** A session id needs a store
to resolve it against, and there is none yet — Postgres is M3.5, SQLite is
M18.1. Rather than ship a replay tool whose only argument is unreachable, v1
takes an append-only JSONL log (`panday_harness::JsonlStore`, one envelope per
line in `seq` order — already the shape of the golden fixtures). The `<session_id>`
form is a lookup in front of the same renderer and lands with the store; nothing
else about the tool changes.

## Milestones

- **M21.1** tracing + OTLP wired in gateway/harness; spans carry the id scheme; local Tempo compose. ✅ *(shipped: `panday_sdk::telemetry`; spans in gateway + harness; `deploy/tempo-compose.yml`.)*

  Span tree as specified: `turn > assemble > model.call > tool.gate >
  sandbox.exec > reduce`, plus `gateway.chat`. OTLP export is opt-in by
  `OTEL_EXPORTER_OTLP_ENDPOINT` — a service with no collector must not spend
  startup failing to reach one — over `http-proto`, so it rides the
  reqwest/rustls stack already present instead of pulling in tonic/grpc.

  **Ids are recorded as raw UUIDs, not `Display`.** `SessionId`'s `Display` is
  the human short form `sess_01J…`, which docs/03 calls "a display encoding, not
  a second id" — recording that would mean a span could not be joined against
  the event log by string equality, which is the whole point of the id scheme.

  **Content-freedom is enforced by test, not by care.** A real turn is run with
  distinctive strings as prompt, assistant text, tool argument and tool result;
  the captured trace is asserted to contain none of them, and every emitted line
  is checked against `FORBIDDEN_CONTENT_FIELDS`. A control test asserts the
  measurements *do* appear — a trace that leaked nothing because it recorded
  nothing would otherwise pass. This is M21.5's audit, applied as soon as the
  spans existed rather than later.

  `PANDAY_DEBUG_CONTENT` with `PANDAY_ENV=production` **refuses to boot**
  (verified: exit 1). A service logging prompts in production is a data
  incident, so the safe failure is not starting.

  **Two bugs found here, both invisible to a naive test.** Spans were attached
  with `span.enter()`, whose guard is *thread-local*: on a multi-thread runtime
  the future moves between polls and the span is silently lost, so events landed
  with no span at all and an operator could not tell which request a failover
  warning belonged to. Both are now `.instrument()`ed. And the first test passed
  against that bug because it used `flavor = "current_thread"`; the suites now
  run multi-thread and attach the subscriber with `WithSubscriber`, which
  follows a future across threads, rather than the thread-local `with_default`.
- **M21.2** Prometheus metrics for the table above; first Grafana board (cost + cache ratio). ✅ *(shipped: `panday_sdk::metrics`, `GET /metrics` on gateway and harnessd, `deploy/grafana-panday.json` + `deploy/prometheus.yml`.)*

  Hand-rolled exposition rather than a metrics facade plus a backend: the format
  is a dozen lines of text, and the two crates it would take are outside the
  docs/02 table. What they would *not* have saved is the only hard part — the
  choice of what to measure.

  **Money is only reported when it is known.** The gateway takes a
  `pricing::CostModel` (default: prices nothing) and a call whose model has no
  configured price increments `panday_unpriced_calls_total` instead of adding
  `$0` to COGS — an invented price on a cost dashboard is worse than a visible
  gap, and the two claims ("free" and "unknown") are not the same. Same rule for
  the reducer: dollars saved needs a price in the loop (M11.4), so today only
  `panday_reducer_tokens_removed_total` is populated, and it is labelled as
  volume rather than passed off as the saving (ADR-007).

  `Pricing` moved from `panday-reducer` to `panday-types` in this milestone. The
  reducer estimates savings with it, the gateway meters COGS with it, and the
  ledger will bill with it; owned by any one of the three, the other two would
  depend on it sideways.

  Error labels are a fixed enum (`rate_limited`, `provider_retryable`, …), never
  the error's `Display` — that carries provider text and ids, which as a label is
  the unbounded cardinality the series cap exists to catch. And the histogram
  bucket bounds were briefly shared through a thread-local, so whichever family
  was constructed last decided the buckets for all of them and a cache-read
  *ratio* was bucketed on latency bounds; each family now owns its bounds, and a
  test asserts two families keep different ones.

  The board is tested against the code: every metric a panel queries must be a
  name `Metrics::names()` reports. A renamed metric otherwise leaves a panel
  reading "No data", which is indistinguishable from a healthy system with no
  traffic.
- **M21.3** `panday replay` v1 (render + time-travel). ✅ *(shipped:
  `panday_harness::replay` + `panday replay <log> [--at|--costs|--verbose|--summary|--diff]`;
  `JsonlStore` for the on-disk log.)*

  `--at seq` truncates rather than reconstructs: a truncated replay is a byte
  prefix of the whole one, because the log at seq N *is* a state the session
  passed through. Tested as that property, since an off-by-one there shows a
  session a user never had.

  Two things the fold forced into the open. `--costs` sums usage from
  `AssistantMessage` only — `TurnFinished.usage` is a redundant turn summary
  (docs/03), and adding both double-bills every turn in the report. And an event
  from a newer version renders as `(unknown event ... )` instead of failing the
  parse: a replay tool that died on a newer server's log would be useless at
  exactly the moment someone reached for it.

  `read_log` refuses a gapped or corrupt log instead of folding it — a fold over
  a hole produces a state no session ever held, and a debugger that invents
  history is worse than no debugger. Every rendered line is anchored to its
  `[seq]` (continuations indented under it) so a finding can be cited.
- **M21.4** Ledger-drift monitor against provider usage reports; alarm plumbing. ✅ *(shipped: `panday_platform::drift`, `panday-platform drift <provider> <report.csv> <from> <to>`, `panday_ledger_drift_micros` + `panday_ledger_drift_direction`.)*

  **Two different questions, and this is the second one.** Internal drift — does the cached balance
  match the sum of the entries — is bookkeeping with a mechanical answer (`balance_drift`,
  `repair_balance`, M17.2). External drift — does what we recorded as COGS match what the provider
  actually billed — has no mechanical answer: a difference means either our metering is wrong or
  the invoice is, and finding out which is a person's job. What the monitor owes them is a number, a
  direction, and enough breakdown to start.

  **Direction is a separate series from magnitude.** Recording *more* than the provider billed may
  mean we over-charged a customer: a refund and an apology. Recording *less* means we are eating the
  difference: a margin problem. Same absolute number, opposite response, so an alert that could not
  tell them apart would page the wrong person.

  **A missing model is louder than a wrong number.** A model on the invoice and absent from our
  ledger is traffic we did not meter at all (`Unmetered`); the reverse is usually an invoice line
  that has not arrived (`Unbilled`). Both are their own class rather than a percentage, because a
  percentage would average an unmetered model away against everything that matched.

  **It compares cost to cost.** `quantity->>'provider_cost_micros'` — what the gateway metered —
  not the credits the customer was charged. Comparing an invoice to a *price* would make the margin
  look exactly like drift, and there is a test whose ledger rows charge double the provider cost
  precisely to catch that mistake.

  **Tolerance exists because rounding is real**: we price per token from our table, they bill from
  theirs, and the two round differently on every call. 1% and one cent by default, both overridable
  — and an absolute floor as well as a percentage, because 40% of two cents is not an incident. A
  malformed line in the report is an error rather than a skipped row: skipping under-counts the
  invoice and reports drift in our favour, which is the direction nobody double-checks.

  **The alerting rule lives in the monitoring system.** This publishes a gauge and logs; a threshold
  compiled into a release is a threshold nobody can change at 3am. The command exits non-zero when
  anything needs a look, so a cron job's own failure handling is the alarm of last resort — a
  monitor whose only output is a log line is a monitor nobody reads.

  **The provider's own CSV is converted outside this binary.** The monitor reads a normalised
  four-column shape, so a vendor renaming a column is a one-line change in a shell script rather
  than a release.
- **M21.5** Content-scrub audit: grep-proof that no content fields leak into spans/logs at default levels. ✅ *(shipped: `crates/panday-sdk/tests/scrub_audit.rs`.)*

  Two halves, because either alone is easy to satisfy and wrong. The **static**
  half reads every `tracing::` macro in the workspace and checks its field names
  against `FORBIDDEN_CONTENT_FIELDS` — that catches a leak the moment it is
  written, in a crate whose tests nobody thought to extend, and on a code path no
  test exercises, which is exactly where a debug log added during an incident
  lives. It is stricter than the milestone asks: level-agnostic, so a leak at
  `trace!` fails too, since a level is a runtime setting and someone will raise it
  while debugging production. The **runtime** half covers the metrics scrape,
  which is scraped by more systems than a trace collector is and is the surface
  usually exposed without auth.

  The audit is self-checking in three ways, because a static analysis that
  silently matches nothing is worse than no analysis: a unit test plants a leak
  and requires the scanner to find it, a second asserts the fields we *do* record
  (`tokens_raw`, `strategy`, `retryable`) are not false positives, and the
  workspace scan fails if it did not reach `gateway.rs`, `actor.rs` and
  `telemetry.rs`. Verified end to end by planting `prompt = "..."` in the harness
  and watching the audit fail with the file and line.

  A deliberate exception is written `// scrub-audit: allow — <reason>` on the line
  above. There are none; the mechanism exists so that adding one is a reviewed act
  rather than a quiet weakening of the test. Metric *label keys* get the same
  treatment — a fixed allowlist, so a new key forces a new cardinality decision
  instead of inheriting the old argument.
