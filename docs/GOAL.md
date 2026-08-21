# Autonomous goal — remaining work is blocked

Laptop-provable leftover clauses closed on 2026-08-20 (`32fbbf2` on
`codeitlikemiley/panday`). Do not re-measure json-bench, re-paste the grok
profile, or re-add the canaries to look busy. Do not invent training or p95
numbers.

If you are handed `/goal`, the remaining work is the blocked table below.
Stop and say so unless the user has provided a host, KVM, Stripe, a corpus,
or a GGUF.

```
/goal Remaining panday numbered work is blocked on hardware, a third party,
or training data. Do not close M19.3/5/7, M22.3–22.5, Stripe HTTP, route-bench
200, or local GGUF profiles without the thing they name. Do not invent rates.
Message the user with the blocked list rather than filling the gap.
```

## Identity of the model plane

- **Grok subscription:** read `~/.grok/auth.json` (xAI OIDC). Call `https://api.x.ai` with that
  bearer. Model id is `xai/grok-4.6`. Implemented in `panday_sdk::oauth`. Never write
  `~/.grok/auth.json`.
- **Claude subscription:** read Claude Code's Keychain item `Claude Code-credentials` or
  `~/.claude/.credentials.json`. Call `https://api.anthropic.com` with `Authorization: Bearer`
  plus `anthropic-beta: claude-code-20250219,oauth-2025-04-20`. Never write those stores.
- **Not an OpenCodex/LiteLLM hop.** Outbound calls go to the provider (`api.x.ai`,
  `api.anthropic.com`, …). Do not send traffic through OpenCodex, LiteLLM, or
  `localhost:8080` as the model.
- **Inbound, the gateway is the compatible front door.** Claude Code, Grok Build, and
  Antigravity CLI (`agy`) point at `panday-gateway` (`docs/11` §Pointing agents). That is
  not "use OpenCodex as upstream".
- **Env:** `PANDAY_BASE_URL` is an optional OpenAI-compatible *upstream* (llama-server, Together).
  `PANDAY_COMPAT_BASE_URL` is a deprecated alias. Inspect-ai talks *to* panday via
  `PANDAY_GATEWAY_URL`. Claude Code uses `ANTHROPIC_BASE_URL` (no `/v1`, plus `--bare`).
  `agy` uses `GOOGLE_GEMINI_BASE_URL` (no `/v1beta`) and `GEMINI_API_KEY` in the process
  environment — it does not read `.env`.
- **Model names** are `provider/model`: `xai/grok-4.6`, `anthropic/claude-sonnet-5`,
  `openai/gpt-5.6-sol`. Never `together/xai/…`.

## Standing rules (handover §1 — non-negotiable)

- No test executes a command targeting an absolute path.
- Untrusted/generated commands run in the T2 jail (`verify_in_jail`). Never fall back to unconfined.
- To show an unguarded variable, print the expansion. Do not execute it.
- Deletes in repo scripts use `${VAR:?}`. Never `rm -rf` a path you did not construct in the same function.
- Ask before any dependency not in `docs/02`. Never publish. `--no-verify` on commits.
- One milestone per commit. `docs/` updated in the same commit when code diverges.
- Never fake a measurement. If hardware is missing, ship the code, name the missing clause, leave the number unstated.

## Closed on this laptop (2026-08-20)

| Item | Evidence |
|---|---|
| Subscription OAuth | `panday_sdk::oauth`; `panday chat -m xai/grok-4.6` → `pong` |
| Phase 1 live fixture | `a_live_model_fixes_it_unattended` ok, 27s, no Anthropic key |
| json-bench (M19.1 leftover) | `scorecards/json-bench-xai_grok-4.6.json` — 200/200 |
| capability profile (M19.2 leftover) | catalog `xai/grok-4.6` is `provenance: measured` |
| M20.1 canaries in agent-bench | 41 tasks; `poisoned-readme` / `poisoned-comment` / `granted-json` |

## Remaining numbered work (blocked)

| Item | Needs | Do not |
|---|---|---|
| Phase 1 exit | The builder using it on a real repo | Treat the fixture loop as the exit. |
| Phase 2 exit | Zed + a real local GGUF | Claim the stranger path is fully proven. |
| route-bench 200 (M19.1 leftover) | Mined traffic (M19.4) | Invent 150 prompts. |
| Local GGUF profiles (M19.2 leftover) | A GGUF on this machine | Guess numbers. Paste grok's row onto a local model. |
| M19.3 Model 1 | Train set ≠ the 50 route-bench prompts; GPU. ≥10pt over 94% heuristic, never confidently wrong, after the model exists. Ask before `ort`/`candle`/`tract`. | Fake the 10pt. Train on the eval. |
| M19.4 10k-pair dataset | Consented transcripts | Mine without `--consent`. |
| M19.5 / M19.7 | After M19.3 and a real corpus. docs/19 go/no-go before GRPO. | Start GRPO to look busy. |
| M19.6 50 tasks in T3 | Mined traffic + KVM | Restore the twelve destructive classes. |
| M22.3 host | `STAGING_DEPLOY_HOST` (and friends). Workflow no-op is the honest state. | Rent a VM unasked. Claim production is live. |
| M22.4 / M14.5–14.6 timing | KVM nodes | Invent p95s. |
| M22.5 air-gap | A machine with no network, following only `INSTALL.md` | Pretend the kit was installed air-gapped. |
| Stripe (M17.4 leftover) | A live key the user already set | Add a Stripe crate and call Stripe without one. |
| Phase 6 | Phases 3–5 saying so | Dashboard / marketplace as filler. |

Grow agent-bench only from mined traffic. The twelve destructive classes are not coming back.

## Out of scope

- OpenCodex as a runtime dependency or default upstream.
- Publishing crates or npm.
- Weakening `no_destructive_fixtures`, `agent_bench::audit`, or `verify_in_jail`.
- Customer dashboard, marketplace, or Phase 6.
