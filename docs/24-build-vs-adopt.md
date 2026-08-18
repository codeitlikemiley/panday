# 24 — Build vs Adopt

The full ledger. "Build" means we own it as product surface; "adopt" means we
depend on it and track upstream; "study" means read their code/tests, take
the lessons, not the dependency.

## Build (the product — where margin and differentiation live)

| What | Why it must be ours |
|---|---|
| panday-harness | THE product. Event-sourced loop, permissions, budgets — the behavior users choose us for. |
| panday-gateway | The commercial choke point: metering, budgets, failover, audit. (ADR-006) |
| panday-router | Routing IS the margin in a subscription business. |
| panday-reducer | Direct COGS lever; cache-aware accounting nobody ships off-the-shelf. |
| AEP protocol | Our event vocabulary; resume/replay/billing hang off it. Only invention allowed. |
| Ledger & entitlements | Money truth. Never outsource truth. |
| T2/T3 sandbox integration | The *policy layer* is ours; mechanisms adopted below. |
| Plugin packaging/signing/registry | Trust wrapper = platform gravity. |

## Adopt (commodities — track versions, send patches upstream)

| What | Choice | Note |
|---|---|---|
| MCP | rmcp (official, v3.x) | tools boundary (ADR-005) |
| Editor protocol | agent-client-protocol crate | 13 editors free (ADR-012) |
| WASM runtime | wasmtime (45+, WASI 0.3) | plugin tier |
| microVM | Firecracker (socket API driven directly) | community SDKs too thin — own a 500-line client |
| Local inference | llama.cpp llama-server (default), mistral.rs (Rust-native alt) | both OpenAI-compat + GGUF |
| GPU serving (phase 5) | vLLM (+ multi-LoRA) | SGLang if prefix-heavy workloads dominate |
| Embeddings | text-embeddings-inference | Rust, OpenAI-compat |
| Training stack | Unsloth / TRL / Axolotl / ART (Python island) | ADR-001; export-capable managed lanes: HF Jobs, Together, Tinker |
| Eval harnesses | inspect-ai, lighteval | through the gateway |
| Billing rails | Stripe (Meters v1; watch v2 credits preview) | ledger stays ours (ADR-009) |
| DB / vectors | Postgres 16 + pgvector 0.8.x | VectorChord/pgvectorscale on scale trigger |
| Observability | tracing + OpenTelemetry + Prometheus/Grafana stack | |
| HTTP/client stack | axum / tower / reqwest-rustls / sqlx | boring on purpose |
| PII scrubbing | Presidio/GLiNER-class models in the training pipeline | |

## Study, don't adopt

| What | Take |
|---|---|
| TensorZero (archived) | gateway internals, eval integration ideas — and the PMF cautionary tale |
| Helicone ai-gateway, Traceloop Hub, Plano | Rust gateway edge cases, provider quirk handling, test corpora |
| rtk | structural compressor patterns per command family; also its measured limits (15) |
| Anthropic sandbox-runtime | T2 mechanism blueprint (bwrap/seccomp/Seatbelt/proxy) — reimplement in Rust |
| Claude Agent SDK / OpenAI Agents SDK | harness ergonomics: hooks, subagents, permission modes, handoffs |
| rig | sans-IO agent-loop structuring for the embedded SDK |

## Explicitly rejected (for now, with the revisit trigger)

| What | Why not | Revisit when |
|---|---|---|
| Kubernetes (day 1) | ops tax before users | T3 fleet ops > 1 day/week |
| Kafka/NATS/Redis/ClickHouse | PG suffices (ADR-003) | trigger table in 22 |
| LiteLLM as gateway | Python in the hot path; we own metering | never (it's a study target) |
| LangChain-style framework layer | the harness IS the framework | never |
| Closed-provider fine-tuning | no weight export; APIs being sunset | if a provider ships weight export |
| Rust-native training | burn/candle not viable for LLM FT in 2026 | check yearly (2027+) |
| Building our own inference engine | llama.cpp/vLLM are a decade of work | never solo |
| Semantic caching by default | correctness hazard for agent traffic | API customers opt in per key |
