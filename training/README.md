# `training/` — the one Python island (ADR-001)

Rust serves models; Python trains and evaluates them. The boundary is deliberate and narrow: this
directory owns dataset builders, eval harnesses and training recipes, and it talks to the rest of
the system over HTTP like any other client.

## Why the evals go through the gateway

`inspect-ai` points at `PANDAY_BASE_URL`, not at a provider. docs/19: evals "hit models **through
panday-gateway** (so evals exercise routing, caching, and adapters — three birds)". An eval that
called Anthropic directly would measure a model we do not ship, in a configuration no user has.

## What runs where

| Suite | Lives in | Needs a model? |
|---|---|---|
| `route-bench` | Rust (`panday_router::bench`) | no — it scores a classifier |
| `json-bench` | Rust (`panday_harness::json_bench`) | yes, to produce a number |
| `reduce-bench` | Rust (`panday_harness::eval`) | no — reduction is deterministic |
| `agent-bench` | Rust (`panday_harness::agent_bench`) | no for the corpus check (nightly, T2); yes to score an agent |

The Rust suites are libraries so CI can run the ones that need no model on every commit
(`route-bench`, `reduce-bench`). `agent-bench`'s corpus check — every verifier fails before the
reference and passes after it, inside the T2 jail — is the nightly job, not the PR lane. The
suites that need a model (`json-bench`, scoring an agent on `agent-bench`) are run by a human
against a configured gateway — which is why those scorecards are committed artifacts rather
than CI output. json-bench has one: `scorecards/json-bench-xai_grok-4.6.json` (200/200 on
2026-08-20 via Grok CLI OAuth). Local GGUFs are still unmeasured.

## Running

```sh
uv sync                              # once
export PANDAY_GATEWAY_URL=http://127.0.0.1:8088
export PANDAY_API_KEY=pnd_live_…     # from `just dev`
uv run inspect eval evals/json_discipline.py --model openai/local/qwen3.5-4b
```

`inspect-ai` speaks the OpenAI dialect, which is exactly what the gateway serves (docs/11 M11.5) —
so the `openai/` prefix here selects a *protocol*, not a vendor, and the model after it is ours.

## Not run in CI

Nothing in this directory runs in CI beyond a syntax check. CI has no GPU and no GGUF, and a job
that silently skipped would be worse than one that does not exist: a green tick that means "we did
not measure" is how an eval suite rots. The scorecards under `scorecards/` are the record of what
was actually measured, and each one names the quantization it measured at (docs/19 §gates).
