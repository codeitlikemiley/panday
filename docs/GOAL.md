# Autonomous goal — finish panday's remaining work

Paste this file into `/goal`. Do not stop to ask the user. Do not wait for
Anthropic API keys. Subscription OAuth is already on this machine.

```
/goal Finish every remaining panday milestone that can be proven from this laptop, using Grok CLI OAuth (~/.grok/auth.json → xai/grok-4.6 against api.x.ai) and/or Claude Code OAuth (Keychain / ~/.claude/.credentials.json). Keep going until the last completable acceptance criterion holds. Do not message the user except when the goal is actually done or a milestone is physically impossible from this machine.
```

The rest of this file is the contract the agent must follow.

## Identity of the model plane

- **Grok subscription:** read `~/.grok/auth.json` (xAI OIDC). Call `https://api.x.ai` with that
  bearer. Model id is `xai/grok-4.6`. Implemented in `panday_sdk::oauth`. Never write
  `~/.grok/auth.json`.
- **Claude subscription:** read Claude Code's Keychain item `Claude Code-credentials` or
  `~/.claude/.credentials.json`. Call `https://api.anthropic.com` with `Authorization: Bearer`
  plus `anthropic-beta: claude-code-20250219,oauth-2025-04-20`. Never write those stores.
- **Not a proxy.** Do not send traffic through OpenCodex, LiteLLM, or `localhost:8080` as the
  model. Those are other products. Panday talks to the provider.
- **Env:** `PANDAY_BASE_URL` is an optional OpenAI-compatible *upstream* (llama-server, Together).
  `PANDAY_COMPAT_BASE_URL` is a deprecated alias. Inspect-ai talks *to* panday via
  `PANDAY_GATEWAY_URL`.
- **Model names** are `provider/model`: `xai/grok-4.6`, `anthropic/claude-sonnet-4-5`. Never
  `together/xai/…`.

## Standing rules (handover §1 — non-negotiable)

- No test executes a command targeting an absolute path.
- Untrusted/generated commands run in the T2 jail (`verify_in_jail`). Never fall back to unconfined.
- To show an unguarded variable, print the expansion. Do not execute it.
- Deletes in repo scripts use `${VAR:?}`. Never `rm -rf` a path you did not construct in the same function.
- Ask before any dependency not in `docs/02`. Never publish. `--no-verify` on commits.
- One milestone per commit. `docs/` updated in the same commit when code diverges.
- Never fake a measurement. If hardware is missing, ship the code, name the missing clause, leave the number unstated.

## What "done" means

A milestone is done when its **stated acceptance criteria** hold, not when something compiles.
Phases exit on their criteria (`docs/23`). Do not start phase N+1 to avoid finishing N, except
that remaining numbered work is already past Phase 1–4 infrastructure.

Prove live behaviour with Grok (`xai/grok-4.6`) or Claude Code OAuth. Run the tests yourself.
Do not ask the user to run them.

## Remaining numbered work

| Item | Do this | Do not do this |
|---|---|---|
| Phase 1 live / dogfood | `cargo test -p panday-harness --test fix_a_failing_test -- --ignored`. Then `panday chat -m xai/grok-4.6` on a real prompt in this repo. Record the result in docs/13 / docs/23. | Ask for `ANTHROPIC_API_KEY`. |
| json-bench number (M19.1 leftover) | Run `just bench` (or `cargo xtask json-bench --write`) against a live gateway whose `xai` adapter is the Grok OAuth session. Commit the scorecard. | Invent a rate. |
| capability profiles (M19.2 leftover) | `cargo xtask profile --model xai/grok-4.6` (and Claude if OAuth works). Paste measured entries. | Guess numbers. |
| M20.1 deferred: canaries in agent-bench | A handful of tasks. Payloads must survive `agent_bench::audit` (no `rm -rf`, no absolute paths). Verifier checks a benign marker file was **not** created. | Copy `canary.rs` payloads as-is. |
| M19.3 Model 1 | Encoder classifier behind `Classifier`, shadow first. Train set is **not** the 50 route-bench prompts. Ask before adding `ort`/`candle`/`tract`. Gate: ≥10pt over the 94% heuristic on route-bench, never confidently wrong, **after** the model exists. | Fake the 10pt. Train on the eval. |
| M19.5 / M19.7 | Only after M19.3 and a real corpus. docs/19 go/no-go before GRPO spend. | Start GRPO to look busy. |
| M22.3 host | If no `STAGING_DEPLOY_HOST`, the workflow's no-op is the honest state. Do not rent a VM with the user's money. | Claim production is live. |
| M22.4 / M14.5–14.6 timing | Code is shipped. Do not invent p95 numbers. | |
| M22.5 air-gap | Kit builder is shipped. Do not pretend an air-gapped machine was used. | |
| Stripe (M17.4 leftover) | Do not add a Stripe crate or call Stripe without a key the user already set. | |

Grow agent-bench only from mined traffic (M19.4). The twelve destructive classes are not coming back.

## Loop

1. Pick the next row that is actually doable from this laptop.
2. Implement it. Run the gate: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace` (or the package under test if the suite hangs on this volume — then still run the affected tests).
3. Commit `--no-verify`, message names the milestone. Push HTTPS as `codeitlikemiley` (`gh auth switch --user codeitlikemiley`, then `git -c credential.helper='!gh auth git-credential' push https://github.com/codeitlikemiley/panday.git HEAD:main`, switch back to `hexuria`).
4. Repeat until no doable row remains.
5. Then, and only then, stop and list: what shipped, what is blocked on hardware/Stripe/a host, and the evidence for each.

## Out of scope

- OpenCodex as a runtime dependency or default upstream.
- Publishing crates or npm.
- Weakening `no_destructive_fixtures`, `agent_bench::audit`, or `verify_in_jail`.
- Customer dashboard, marketplace, or Phase 6.
