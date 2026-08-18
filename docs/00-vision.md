# 00 — Vision & Product

## What we are building

A vertically integrated AI platform, owned end to end, written in Rust, that
you can sell as a subscription, expose as an API, run entirely offline, and
eventually power with your own trained models.

The strategy is **"all of it, phased"** — one infrastructure, three product
surfaces that light up in order:

1. **A coding agent, CLI-first.** The wedge. It exercises every hard component
   on day one (harness, sandbox, reducer, plugins, gateway) and you dogfood it
   to build the rest of the platform. Speaking ACP makes it usable from 13
   editors the day it works.
2. **The platform as a product.** The same gateway, router, harness-as-a-service
   and SDK, sold to other developers API-first. Your agent becomes reference
   customer #1.
3. **Consumer surfaces.** Web/desktop chat with the plugin marketplace, built
   on the platform API like any third party would.

## Why this can exist at all

The 2026 landscape settled three questions in our favor:

- **The protocols are open.** MCP for tools (official Rust SDK, `rmcp`), ACP
  for editor↔agent (official Rust crate), OpenAI-compatible HTTP for model
  backends, SKILL.md-style markdown for skills. We adopt all four and compete
  on runtime quality, not protocol lock-in.
- **The models are fungible.** Commercial APIs, open weights behind vLLM, and
  GGUF on a laptop all speak the same interface. A gateway + router that
  treats them uniformly is the position of maximum leverage.
- **The incumbents proved the shape.** Claude Code / Agent SDK demonstrated the
  harness pattern (event stream, hooks, subagents, permissions, skills).
  We are not inventing the category; we are building an owned, Rust, self-
  hostable instance of it.

And one cautionary tale: TensorZero — a well-funded, well-engineered Rust LLM
gateway — was archived in June 2026; its founder publicly cited failure to
find product-market fit. Standalone gateways *can* be businesses (LiteLLM and
Helicone sell exactly that), but it is a crowded commodity market. The lesson
for us: **our gateway is not the product — it is the engine room of one**,
and it wins by serving our agent and platform, not by competing on gateway
features.

## Product principles

**Own the choke points, adopt the commodities.** We build the harness, gateway,
router, reducer, and billing — the places where margin, differentiation, and
data live. We adopt MCP, ACP, wasmtime, Firecracker, llama.cpp/mistral.rs,
Stripe, OpenTelemetry, and the Python training stack — places where building
is ego, not edge. `24-build-vs-adopt.md` is the full ledger.

**The token bill is an engineering target.** Every tool result passes a
reducer; context layout is prompt-cache-aligned; savings are measured against
real cache economics (cache reads ~0.1x; Anthropic-style cache writes carry a
1.25x–2x surcharge by TTL — naive "compression" that churns a cached prefix
*loses* money). This discipline,
applied end to end, is a durable cost advantage in a subscription business.

**Event-sourced everything.** A session is an append-only event log. Replay is
debugging, resume is replay, branching is forking a log, audit is reading it,
and — critically — **training data is mining it** (with consent). The log is
the platform's compounding asset.

**Offline is a deployment target, not a feature flag.** One binary (`ferrum
local`) runs the harness, a gateway-lite, and a GGUF model server on a laptop
with zero outbound calls. It is how you sell to regulated buyers, and it is
the forcing function that keeps the architecture honest.

**Evals before training, always.** No model gets trained before the eval that
would judge it exists. No trained model ships without beating the incumbent on
that eval — including after GGUF quantization.

## Business model

| Channel | What they buy | Metered on |
|---|---|---|
| Subscription (Pro/Max tiers) | The agent + surfaces, generous included usage | internal credit ledger; plan caps |
| API / platform | Gateway, harness-as-a-service, SDK | tokens + sandbox-seconds + storage, prepaid credits |
| Enterprise / on-prem | The offline distribution + support | seats + annual license (signed entitlement tokens) |

All three run on the same metering pipeline (`17-platform.md`). Margins come
from routing (cheap models for cheap work), caching, and the reducer; the
subscription is priced against *reduced* cost, not list price.

## What success looks like

- **Phase 1 exit:** you use your own agent daily, by preference, for real work.
- **Phase 3 exit:** a stranger pays money; usage → ledger → Stripe reconciles
  to the cent; killing a provider mid-request degrades, not breaks.
- **Phase 5 exit:** a model you trained handles ≥30% of routed traffic at
  equal-or-better eval scores and lower cost than the provider it displaced.

## Naming

`Ferrum` (Fe, iron — the thing rust comes from) is a **placeholder codename**
chosen for grep-ability. Rename before anything public:
`grep -rl ferrum . | xargs sed -i 's/ferrum/newname/g'` and rename the crate
directories. Check crates.io/npm/domain availability before you commit.
