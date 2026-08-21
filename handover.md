# Handover — panday

Written 2026-08-20. Read the **Rules** section before touching anything; it exists because a
previous session destroyed part of the developer's machine.

---

## 1. Rules — non-negotiable

### 1.1 Never execute a destructive command, and never write one into a fixture

**What happened.** A session was building `agent-bench` (docs/19 M19.6): a corpus of "here is a bug,
prove it's a bug" tasks. One task's subject was the bug class *"shell script deletes the root when a
variable is unset"*, so its fixture was:

```sh
rm -rf "$DIR/"*
```

and its verifier ran that script with `DIR` unset, to demonstrate the bug. With `DIR` empty the shell
expands that to `rm -rf /*`. The test suite executed it. macOS SIP and TCC blocked most of it, but
what died: **~18 application bundles** (VS Code, Obsidian, Alfred, AlDente, Edge, Teams, ChatGPT,
Android Studio, Xcodes, IINA, DBngin, TG Pro, Reminders MenuBar, iOS App Signer, Codex, WezTerm,
Ollama, Kiro), the **ssh-agent socket** under `/var/run/com.apple.launchd.*/Listeners`, and the
running terminal — whose deletion invalidated its code signature, which made macOS revoke the
session's TCC grants, which made the repository unreadable for hours.

**Why it happened** (all four failures compounded; the fourth is the one to internalise):

1. Confusing *where a process runs* with *what it can touch*. The verifier ran with
   `current_dir(temp_workspace)`, which felt like containment. The command's target was absolute, so
   the cwd was irrelevant.
2. Writing a fixture *about* catastrophic deletion and then running it unsandboxed — in a repo that
   ships T0/T1/T2 sandboxes for exactly this.
3. Generating 50 fixtures with a script and reviewing them for *shape* (unique ids, non-empty
   prompts) instead of asking of each one: **what does this do when executed?**
4. Adding a safety-flavoured detail (`env_clear()` for hermeticity, with a comment about it) and
   letting it stand in for actual safety. Environment isolation is not filesystem isolation.

**The rules now:**

- **No test may execute a command that targets an absolute path.** Fixtures name paths *relative* to
  their own workspace, or they do not exist.
- **Anything that runs untrusted or generated commands runs inside the T2 jail.** Use
  `panday_harness::agent_bench::verify_in_jail` as the pattern: it scopes writes to one directory and
  returns `BenchError::NoJail` where no tier exists rather than falling back to running unconfined.
  A benchmark that degrades to "run it directly" is the original bug.
- **To demonstrate an unguarded variable, inspect the expansion — don't execute it.**
  `DIR= sh -c 'echo rm -rf "$DIR/"*'` shows the problem and destroys nothing.
- **`${VAR:?}`, never `"$VAR"`, as the target of any delete in a script.** The guarded form aborts
  when the variable is unset *or empty*. Both repo scripts (`scripts/backup-drill.sh`,
  `scripts/build-rootfs.sh`) were corrected this way.
- **Never `rm -rf` a path you did not construct in the same function.** `Workspace::drop` checks
  `starts_with(temp_dir())` before removing anything.

**Three guard rails now enforce this — do not weaken them:**

| Guard | Where | What it does |
|---|---|---|
| Repo-wide lint | `crates/panday-sandbox/tests/no_destructive_fixtures.rs` | Fails `cargo test` if any `.rs`/`.sh`/`.yml` file contains a delete targeting `/` or `/*`, a delete of an unguarded variable, a recursive chmod/chown from root, `mkfs`, a block-device overwrite, or a fork bomb. Exemption is per file, needs a stated reason, and is greppable (`dangerous-strings: data-only`). Eight files carry it — the filter denylist, the permission rules, the injection canaries, and the tests asserting those get vetoed. **`${VAR:?}` is accepted deliberately**: the lint must accept the fix, or it teaches people to add exemptions instead of correctness. |
| Corpus audit | `panday_harness::agent_bench::audit` | Refuses any task naming an absolute path, `..`, or a destructive verb — *before* anything executes. Tested against the original fixture, constructed as data and rejected. |
| The jail | `verify_in_jail` | Containment is a property of *where* the command runs, not of how carefully the fixture was written. A test proves it by having a verifier try to write into a neighbouring directory and asserting the file is unchanged. |

### 1.2 Other standing rules from the user

- **Ask before adding any network-touching dependency.** (Approved so far: reqwest, tokio,
  serde_yaml_ng, sha2, regex, toml, ed25519-dalek, tar, flate2, tokio-tungstenite, rmcp, trybuild.)
- **Never publish anything** — no crates.io, no npm. The TypeScript SDK is generated with
  `"private": true` on purpose.
- **Don't delete folders or files that aren't yours to delete.** After the incident: assume nothing,
  verify the path, prefer moving to deleting.
- Commits use `--no-verify` (the pre-commit hook has false positives; the user approved this).
- A red build is acceptable to hand over on — say so rather than hiding it.
- **Open PRs as draft; do not mark ready or merge until every CI job is green.** This rule exists
  because M25.1 went straight onto `main` with failing tests and the fixes had to be pushed to `main`
  after it. `main` has **no branch protection** (`gh api …/branches/main/protection` → 404), so
  nothing on GitHub enforces this — the discipline is the only guard. Check the run is on the head
  SHA, not a stale one; that has bitten this repo before.
- One milestone per commit. If the work exposes a gap with no milestone number, **add the milestone**
  to the spec rather than smuggling the work into an unrelated commit (CLAUDE.md §4).

---

## 2. Where the project stands

**HEAD (`main`):** `3cfcc4f` — *experiment: rotate Grok logins and provider API keys (#10)*
**Remote:** `git@github.com:codeitlikemiley/panday.git` (public, user `codeitlikemiley`)
**CI:** green on that commit.

**101 milestones total: 86 shipped ✅, 15 remaining.** Recount it rather than trusting this line —
`docs/25` added twelve milestones after the "89" figure was written, and CLAUDE.md §4 quotes the
count too.

| Spec | Shipped |
|---|---|
| 02-workspace, 03-protocol, 10-sdk, 11-gateway, 12-router, 13-harness, 14-sandbox, 15-reducer, 16-plugins, 17-platform, 18-local, 20-security, 21-observability | **all** |
| 19-training | 4 / 7 (M19.1, M19.2, M19.4, M19.6). M19.3 / M19.5 / M19.7 not started. |
| 22-deployment | 2 / 5 shipped (M22.1, M22.2). M22.3 / M22.4 / M22.5 partial. |
| 25-credentials | 3 / 12 shipped (M25.1 vault, M25.2 `panday creds`, M25.4 pooled adapter). M25.9 partial. M25.3 is PR #12; M25.5–M25.12 are the next work. |

**The operator track (`docs/25`) is new since the last handover.** Twelve milestones to pool upstream
API keys and Grok/Claude/Codex subscriptions behind the gateway and rotate between them. It is
additive — it does not skip training, Stripe, or the hardware-blocked work below. `docs/23` §Operator
track is the summary; `docs/25` is the spec.

Phases 0–2: numbered milestones complete; Phase 1's *exit* still wants the builder's dogfood
judgement, Phase 2's *exit* still wants Zed and a real GGUF. Phase 3 complete except Stripe and a
host. Phase 4 complete except T3 on real hardware. Phase 5: all infrastructure, no trained models.
Phase 6 not started.

**Laptop-provable leftover clauses closed 2026-08-20** (do not re-run them to look busy):

- Subscription OAuth in `panday_sdk::oauth`: Grok CLI `~/.grok/auth.json` → `xai/grok-4.6` against
  `https://api.x.ai`; Claude Code Keychain / `~/.claude/.credentials.json` against Anthropic. Outbound
  is not a third-party proxy. Inbound, Claude Code / Grok Build / Antigravity CLI (`agy`) point at
  `panday-gateway` — how: `docs/11` §Pointing agents (`--bare`, no `/v1` / `/v1beta` on those
  base URLs). Model ids are `provider/model`. Env: `PANDAY_BASE_URL` (alias
  `PANDAY_COMPAT_BASE_URL`) is an *upstream*; inspect-ai talks *to* panday via `PANDAY_GATEWAY_URL`.
- Live M13.2: `a_live_model_fixes_it_unattended` ok (27s, no `ANTHROPIC_API_KEY`). `panday chat -m
  xai/grok-4.6` replied `pong` twice.
- M19.1 json-bench: **200/200** on `xai/grok-4.6` (`scorecards/json-bench-xai_grok-4.6.json`).
  Frontier model on a 4B-sized corpus — not a GGUF-after-quantize number. route-bench stays at 50.
- M19.2: catalog row `xai/grok-4.6` is `provenance: measured` (json 1.00, tools 1.00, usable context
  64528 under a 65536 ceiling). Local GGUF rows stay `declared`.
- M20.1 deferred half: three agent-bench canaries (`poisoned-readme`, `poisoned-comment`,
  `granted-json`); corpus is **41 tasks in T2**.

### 2.1 The 3 partial milestones — code done, hardware missing

| Milestone | Built and tested | Blocked on |
|---|---|---|
| **M22.3** Production live + status page | `GET /status` (unauthenticated, content-free, 503 when degraded, reports build + database + schema); backup/restore drill run and recorded in docs/20 | A host to deploy to |
| **M22.4** T3 pool on KVM + chaos test | `panday_sandbox::t3::nodes` — consistent-hash ring, `drain`; failover suite proving a session resumes on another node by folding its log (`crates/panday-harness/tests/node_failover.rs`) | KVM nodes, for the timing half |
| **M22.5** Air-gap bundle from its README alone | `xtask::airgap` — builder *refuses* to emit a kit whose installer contains a network command; README checked against the box and against the env vars the binary actually reads | A machine with no network |

### 2.2 The 3 not started — all need GPUs and data

- **M19.3** Model 1: router classifier beating the heuristic by ≥10pt on route-bench.
- **M19.5** Model 2: summarizer in the reducer, ≥25% cheaper than the pool it replaces, GGUF in catalog.
- **M19.7** Model 3 v1: coding specialist SFT, then a go/no-go on the GRPO spend.

Everything upstream of them exists: eval spine + a measured json-bench card (M19.1), capability-profile
generator with one measured row (M19.2), shadow-mode classifier comparison (M12.5), consent-first
transcript mining (M19.4), agent-bench as the RL environment (M19.6, 41 tasks in T2), signed model
catalog for a tuned GGUF (M18.2).

### 2.3 Leftover clauses on shipped milestones (do not close with invented numbers)

| Clause | Honest state |
|---|---|
| M19.1 route-bench 200 | At 50. Remaining 150 from mined traffic (M19.4), not invention. |
| M19.2 local profiles | `xai/grok-4.6` measured; local GGUF rows `declared`. |
| M19.4 10k-pair dataset | Pipeline shipped. Repo has no consented transcripts. |
| M19.6 50 tasks in T3 | 41 in T2. Twelve destructive classes stay gone. Grow from mined traffic. |
| M14.5 / M14.6 p95 | Code shipped. Cold 300ms / warm 50ms unmeasured (need KVM). |
| M17.4 Stripe HTTP | Inbox/projection shipped. No Stripe crate, no live key. |
| M22.2 staging deploy | Workflow no-op until `STAGING_DEPLOY_HOST` is set. |
| Phase 1 exit | Live fixture loop works. “Your repo, and you reach for it tomorrow” is the builder's. |
| Phase 2 exit | Zed unverified; live llama-server leg `#[ignore]`d. |
| Phase 6 | Dashboard / marketplace. Out of scope until 3–5 say so. |

---

## 3. In-flight — two stacked PRs, neither merged

`git status` on `main` is clean; nothing is sitting uncommitted. But two branches are out for review,
both **draft on purpose** under the rule in §1.2:

| PR | Branch | Milestone | State |
|---|---|---|---|
| [#12](https://github.com/codeitlikemiley/panday/pull/12) | `m25.3-headers` | M25.3 | Draft. All six CI jobs green on head `71b9082`, `mergeStateStatus: CLEAN`. |
| [#13](https://github.com/codeitlikemiley/panday/pull/13) | `m11.7-retry-after-egress` | M11.7 | Draft, based on `m25.3-headers` — GitHub retargets it to `main` when #12 merges. |
| [#15](https://github.com/codeitlikemiley/panday/pull/15) | `m11.8-one-error-mapper` | M11.8 | Draft, based on `m11.7-retry-after-egress`. Green on `f2bc801`. |

**Merge in order: #12 → #13 → #15.** Each retargets to `main` as its base lands; out of order, a
diff reads as if it contains its parent's changes too.

- **#12 / M25.3** — the transport stops dropping response headers. `post_sse` returns
  `SseResponse { headers, body }`; a 429's `Retry-After` fills `RateLimited.retry_after_ms`, which was
  hardcoded to 0. Two defects were found and fixed *before* it was pushed, and both are worth knowing
  because the same shapes will recur: a plain `min()` over a pool treated the 0 sentinel ("no header")
  as the soonest wait and erased every real one, and filling `retry_after_ms` activated a
  previously-dead retry arm that slept an unbounded upstream-controlled value outside the timeout
  layer. `MAX_HONOURED_RETRY_AFTER` (60s) now caps what we will sleep.
- **#13 / M11.7** — the gateway *tells the client* the wait. Until this, every 429 it ever emitted was
  a bare status: the value was computed and dropped at the HTTP boundary, and panday's own
  `RateLimiter` had been discarding its own window remainder the same way. Note for anyone touching
  error handling: **there are three independent error mappers** — `ingress::error_response`,
  `messages::anthropic_error`, `gemini_api::gemini_error` — that share no code and have already
  drifted. Fixing one is not fixing the ingress — a change to `error_response` alone reaches neither
  Claude Code nor Antigravity.
- **#15 / M11.8** — closes that drift. Status was 503/404/404 for `ModelUnavailable`, 403/502/502 for
  `PermissionDenied`, 402/402/502 for a budget refusal. The `PermissionDenied` row was the dangerous
  one: 403 is terminal and 502 is retryable, so a client with ordinary backoff would hammer a refusal
  it can never satisfy — on the two routes agents actually use. Status now comes from one
  `ingress::status_for`; envelopes stay per-dialect. Two table-driven tests hold the line, and both
  fail against #13, which is what makes them a guard rather than decoration.

  **Still open, and now the interesting one:** `ModelUnavailable` conflates two failures — an
  exhausted chain (503 is right) and a rule with no usable target, which includes a caller pinning a
  model this deployment will never serve. 503 tells that caller to come back later about a request
  that can only ever fail; all three upstream APIs would answer 404. Splitting the variant touches
  the error vocabulary in `docs/10` and the generated clients, so it is a milestone, not a rider.
  Written up under docs/11 M11.8.

---

## 4. Known problems

### 4.1 CI on `main` is green

The T1 hook 10ms flake on `c300fb1` (run `32330143170`) was a **timing flake, not a regression**:
docs/16 gives a wasm hook a 10ms wall-clock budget, and asserting behaviour against that while a
shared runner executes 900 other tests measures the runner. Fixed in `5fdf3f7` by splitting
behaviour (2s) from budget tests, with retries on the budget pair. Current HEAD (`32fbbf2`) is
green on `ci` / `release` / `deploy`.

### 4.2 SSH push is broken — use HTTPS

The ssh-agent socket was destroyed in the incident. The keys are intact
(`~/.ssh/id_codeitlikemiley`) but passphrase-protected, and the agent that held them is gone. Push
as `codeitlikemiley`:

```sh
gh auth switch --user codeitlikemiley
git -c credential.helper='!gh auth git-credential' \
  push https://github.com/codeitlikemiley/panday.git HEAD:main
gh auth switch --user hexuria
```

The user can restore SSH with `eval "$(ssh-agent -s)" && ssh-add --apple-use-keychain ~/.ssh/id_codeitlikemiley`.

### 4.3 Machine state after the incident

- **Apps to reinstall** (user's task, commands in the git log / earlier transcript):
  `brew reinstall --cask visual-studio-code obsidian alfred aldente microsoft-edge microsoft-teams chatgpt android-studio xcodes-app iina dbngin tg-pro reminders-menubar ios-app-signer codex`
  plus `brew install --cask wezterm`; Ollama and Kiro need manual downloads.
- **Docker is fine** — an earlier audit claiming otherwise was wrong. Both compose stacks run and the
  `panday-dev_pgdata` volume survived (7 accounts, 52 ledger entries).
- **Unverified**: `~/Documents`, `~/Desktop`, `~/Downloads`. The agent is TCC-blind to these; the
  user must check them in Finder. `~/Code` was already empty before the incident (mtime Jul 19).
- If the volume goes unreadable again (`Operation not permitted` on every read while `stat` works),
  it is TCC: grant Full Disk Access to the host app and **restart it** — a running process keeps its
  cached denial.

### 4.4 Workspace nextest can hang on this volume

`cargo nextest run --workspace` has hung listing binaries on this volume filesystem. Substitute
package-scoped `cargo test -p …` / `cargo clippy -p …` and say so, rather than waiting on the hang.

---

## 5. How to work in this repo

### 5.1 The working agreement (from CLAUDE.md)

- `docs/` is the **source of truth**. When code diverges from a spec, the spec is updated **in the
  same commit** — including what was learned and what went wrong. Every shipped milestone carries a
  ✅ with the notes.
- **One milestone per session/commit**, tests and clippy green before moving on. Do not start N+1 to
  avoid finishing N.
- Nothing outside docs/02's dependency table without asking.
- Read order for a newcomer: `docs/00-vision.md` → `01-architecture.md` → `04-decisions.md` (the
  ADRs) → `23-roadmap.md` → the component spec you are working in.

### 5.2 The gate — run all of it before committing

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # -D warnings is enforced
cargo nextest run --workspace                            # ~920 tests, ~25s warm
cargo deny check
cargo xtask schemas --check     # proto/ must match the Rust types
cargo xtask sbom --check        # sbom.cdx.json must match the lockfile
cargo xtask ts-sdk --check      # sdk/typescript must match the schemas + OpenAPI
```

`just check` runs fmt + clippy + tests. Regenerate derived artifacts with `just generated`.

### 5.3 The Postgres integration lane

`#[ignore]`d tests need a database:

```sh
docker compose -f deploy/integration-compose.yml up -d --wait
PANDAY_TEST_DATABASE_URL=postgres://panday:panday@127.0.0.1:5433/panday_test \
  cargo nextest run --run-ignored all -E 'package(panday-platform)'   # 137 tests
```

Three databases on three ports, deliberately: **5432** is whatever the developer already runs,
**5433** is the disposable integration lane (tmpfs, fsync off), **5442** is `just dev` (named volumes,
holds a minted API key). `just it` and `just dev` must never touch each other.

### 5.4 The dev stack

```sh
just dev      # compose up, migrate, mint an account + API key, print the curl
just serve    # panday-platform on the host against that database
just drill    # backup/restore drill
just airgap   # build the offline kit (needs release binaries)
just bench    # json-bench against a running gateway (Grok CLI OAuth is enough)
```

### 5.5 Conventions worth matching

- **Tests are named as sentences that state the property**, and carry a comment saying *what failure
  they prevent*. Look at `crates/panday-platform/src/keys.rs` or `t3/pool.rs` for the register.
- **Comments explain the decision, not the mechanics** — why this way, what the alternative would
  cost, and what a test caught. Several of the best comments in the tree exist because a test found a
  bug; keep recording those.
- **Money is integers** (micro-dollars / credit-micros). Never floats. Rounding happens once, at an
  edge.
- **Never fake a measurement.** If something needs hardware that isn't here, ship the code, name the
  missing clause in the spec, and leave the number unstated. See M19.1, M22.2, M14.5 for the pattern.
- Hand-rolled over a dependency when the format is documented and small (the Prometheus exposition,
  the CycloneDX SBOM, the TS emitter, the Firecracker client, the consistent-hash ring).

---

## 6. Suggested next steps, in order

**This is no longer true: the operator track is laptop-buildable and unfinished.** Everything below
item 1 still needs hardware, a third party, training data, or a judgement only the builder can make —
but M25.5 onward needs none of that. Do not invent rates or p95s for the blocked items.

0. **Merge #12, then #13** (§3), then continue the operator track in spec order: **M25.5** sticky
   `session_id` + per-credential breakers, **M25.6** operator ceiling / local counters / remaining %
   on `/accounts`, **M25.7** overlay the ratelimit headers M25.3 now keeps onto those counters,
   **M25.8** `most_remaining` selector, **M25.9** finish vault-as-boot-source, **M25.10** prove Codex
   import against the OpenAI adapter *or* write the note saying it does not work. M25.11 (hosted
   Postgres ciphertext) and M25.12 (Keychain-wrapped KEK — needs `keyring`, so §1.2 applies) are
   skip-unless-asked. There is also one open question with no milestone number yet: `ModelUnavailable`
   answers 503 for a model this deployment will never serve, where 404 is the honest answer (§3).
1. **Phase 1 dogfood (the builder, not an agent).** Use the agent on a real repo under `dev` and
   decide whether you reach for it the next day. The fixture loop already passed live.
2. **A host (M22.2 leftover / M22.3).** The deploy workflow is written and guarded on
   `STAGING_DEPLOY_HOST`; set that secret plus `STAGING_SSH_KEY`, `STAGING_SMOKE_URL`,
   `STAGING_DATABASE_URL` and it runs, with the smoke suite gating the deploy. Do not rent a VM
   with the user's money unasked.
3. **KVM (M14.5–14.6, M22.4).** Two nodes, then measure fold latency. Do not invent p95s.
4. **Air-gap (M22.5).** Install the kit on a machine with no network, following only `INSTALL.md`.
5. **Stripe (M17.4 leftover / Phase 3 exit).** Live key, then the HTTP client and signature check.
   Do not add a Stripe crate without a key the user already set.
6. **Training (M19.3, then M19.5 / M19.7).** Labelled corpus that is *not* the 50 route-bench
   prompts; GPU-hours; docs/19 go/no-go before GRPO spend. The `Classifier` trait and shadow
   harness are the slot Model 1 drops into.
7. **If growing agent-bench back toward 50 tasks**, take them from mined traffic (M19.4), not from
   invention — and re-read §1.1 first. The twelve tasks removed were the destructive classes; they
   are not coming back.
