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

Prometheus endpoint per service; Grafana dashboards checked into `deploy/`.

## Logs

`tracing` JSON to stdout, shipped by the platform (Loki/Vector at first).
Content-free by default; `FERRUM_DEBUG_CONTENT=1` per-service for local dev
only (refuses to start with it set in `env=production`).

## The replay tool (the killer feature)

`ferrum replay <session_id>` — renders any session's event log as the CLI
would have shown it, with `--at seq` time travel, `--diff` between two
replays (e.g., before/after a reducer change), and `--costs` per-turn ledger
overlay. Built once in phase 2 against the fold; pays for itself the first
week. This tool is why state-must-fold-from-log is an invariant and not a
preference (ADR-002).

## Milestones

- **M21.1** tracing + OTLP wired in gateway/harness; spans carry the id scheme; local Tempo compose.
- **M21.2** Prometheus metrics for the table above; first Grafana board (cost + cache ratio).
- **M21.3** `ferrum replay` v1 (render + time-travel).
- **M21.4** Ledger-drift monitor against provider usage reports; alarm plumbing.
- **M21.5** Content-scrub audit: grep-proof that no content fields leak into spans/logs at default levels.
