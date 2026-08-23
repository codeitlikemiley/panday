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
  serde_yaml_ng, sha2, regex, toml, ed25519-dalek, tar, flate2, tokio-tungstenite, rmcp, trybuild,
  keyring + apple-native-keyring-store — approved 2026-08-22, macOS-only, for the vault KEK.)
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
- **Never `gh pr merge --delete-branch` on a PR that another PR is stacked on.** Deleting the base
  branch auto-closes the child, and a closed PR whose base is gone can be neither reopened nor
  retargeted — the work has to be rebased and re-filed under a new number. Delete base branches by
  hand once their children have landed. (This is how PR #13 was lost: it had to be rebased onto the
  squashed `main` and re-filed as #16.)
- **Check who `gh` is with `gh api user -q .login`, not `gh auth status`.** The
  active account drifts back to `hexuria`, which has read but no write here, and
  every `gh pr create` then fails with `must be a collaborator`. Parsing local
  status output gave the wrong answer twice on 2026-08-23; asking GitHub who you
  are does not.
- **`cfg`-gated code cannot be verified locally, so plan a CI round trip.**
  `x86_64-unknown-linux-gnu` is installed, but cross-compiling still fails —
  `ring` wants `x86_64-linux-gnu-gcc` and there is no C cross-toolchain here.
  M25.12 was written entirely under `cfg(target_os = "macos")`, the local gate
  only ever compiled the macOS half, and the Linux branch reached CI having never
  been built. Five of six jobs passed; the one that failed was the only one that
  had looked at it.
- **The integration lane is not in the local gate.** `panday-platform`'s smoke
  suite runs only in CI's `integration` job, and a status-code change nearly went
  red there right after a local "all green". Run it by hand when touching
  anything an HTTP status assertion could observe:
  `docker compose -f deploy/integration-compose.yml up -d --wait`, then
  `PANDAY_TEST_DATABASE_URL=… cargo nextest run --run-ignored all -E 'package(panday-platform)'`.
- **Never `pkill -f "cargo test"`.** It is not scoped to this repo and will reach
  another project's run on the same machine. Kill by recorded PID.
- **If `git fetch` fails, start the ssh-agent — do not reach for HTTPS** (§4.2). A failed fetch
  leaves `origin/main` stale, and a stale `origin/main` will happily let you rebase onto the wrong
  base; that near-miss is what made this look like a remote problem rather than a missing agent.

---

## 2. Where the project stands

**HEAD (`main`):** `7dd4616` — *docs: make the specs stop lying, and number three invisible items (#30)*
**Remote:** `git@github.com:codeitlikemiley/panday.git` (public, user `codeitlikemiley`)
**CI:** green on that commit.
**Open:** PR #31 (M25.11 + a billing test fix), all six jobs green, awaiting merge — see §3.

**107 milestones total: 98 shipped ✅, 9 remaining** (counting #31, which is green but unmerged). The total rose from 104 without any work being added: M0.2, M11.10 and M14.8
were always-real items that carried no number, so they were invisible to this
count. Recount it rather than trusting this line —
`docs/25` added twelve milestones after the "89" figure was written, and CLAUDE.md §4 quotes the
count too.

| Spec | Shipped |
|---|---|
| 02-workspace, 03-protocol, 10-sdk, 11-gateway, 12-router, 13-harness, 14-sandbox, 15-reducer, 16-plugins, 17-platform, 18-local, 20-security, 21-observability | **all** |
| 19-training | 4 / 7 (M19.1, M19.2, M19.4, M19.6). M19.3 / M19.5 / M19.7 not started. |
| 22-deployment | 2 / 5 shipped (M22.1, M22.2). M22.3 / M22.4 / M22.5 partial. |
| 25-credentials | **12 / 12 shipped** (M25.11 in PR #31). M25.1 vault, M25.2 `panday creds`, M25.3 transport headers, M25.4 pooled adapter, M25.5 per-credential breakers + sticky sessions, M25.6 ceiling/counters/remaining %, M25.7 header overlay, M25.8 `most_remaining` + funnel, M25.9 vault-as-boot-source, M25.10 Codex importer proven, M25.11 hosted Postgres ciphertext, M25.12 Keychain-wrapped KEK (`keyring` approved 2026-08-22). M25.11 was never blocked on "needs a Postgres" — `deploy/integration-compose.yml` has run one all along. |

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
| **M22.5** Air-gap bundle from its README alone | `xtask::airgap` — builder *refuses* to emit a kit whose installer contains a network command; README checked against the box and against the env vars the binary actually reads | **Nothing external.** `docker run --network none` is a real air gap for every property this kit claims. What is missing is proof the install *succeeds* without a network — every asserted property today is negative, enforced at build time by `reaches_network` in `xtask/src/airgap.rs`. Also unpacked: no real GGUF has ever gone through `models/`. |

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

## 3. In-flight — PR #31, green, awaiting merge

https://github.com/codeitlikemiley/panday/pull/31 — branch `m25.11-pg-vault`, two commits, all
six CI jobs green including `integration`. It is still a **draft**: `gh pr ready` and `gh pr merge`
were both refused by the permission classifier, so marking it ready and merging is the user's to do.

**Commit 1 — M25.11.** The deliverable is the *conformance suite*, not the store.
`MemoryStore` and `SqliteStore` had disjoint test sets — each trusted for something the other had
never been asked to do — so `CredentialStore`'s contract was unknown and a third implementation
would have been guesswork. `panday_sdk::vault::conformance::run` is nine invariants; both existing
stores passed unmodified, and `PgStore` then fell out as `sqlite.rs` with `$1` and `bytea`.
Rows are operator-global on purpose, stated in `0009_credentials.sql`. Hosted KEK provisioning is
`PANDAY_VAULT_KEY` and only that — documented in docs/25, because the fall-through *generates* a
key and every restart would then mint one that cannot read the last one's rows.

**Commit 2 — two pre-existing integration-lane defects**, found while verifying commit 1, the
second masked by the first. Three billing assertions read `ApplyReport`'s fleet-wide counters to
prove a row-scoped fact, and a concurrent test's drain zeroed them. With those gone, a second test
failed deterministically: it paged `billing::stuck()` for its own row, but that orders
most-attempted-first and the shared database holds 231 unapplied events at up to 23 attempts, so a
fresh row sorts past any limit. It got likelier to fail every time anyone ran the lane.

**Also fixed:** `EMBEDDED_MIGRATIONS` had no `0009`. A deployed service reads the compiled-in set,
not `migrations/`, so it would have booted with no `credentials` table while `/status` called the
schema unhealthy — the exact failure that list's comment says a test exists to catch.

The operator track (`docs/25`) is finished end to end: pool several credentials
per provider → measure what each has left, from both the operator's declared
grant and the provider's own response headers → prefer the fullest → optionally
omit a provider whose credentials are all spent → persist the lot in the sealed
vault so the next boot finds it.

**Five findings from building it, kept because each is a shape that will recur:**

- **A sentinel is not a value.** `retry_after_ms == 0` means "no upstream said",
  and a plain `min()` over a pool collapsed to it the moment one member stayed
  silent — erasing every real wait its siblings reported. Unknowns must abstain
  from an aggregate, never win it.
- **Filling a field can arm dead code.** `retry_after_ms` had always been 0, so
  the retry middleware's server-stated-delay arm had never run. Populating it
  activated an unbounded, upstream-controlled `sleep` sitting outside the
  timeout layer.
- **There are three error mappers, not one** — `ingress::error_response`,
  `messages::anthropic_error`, `gemini_api::gemini_error`. They shared no code
  and had drifted on status. Fixing the one you happen to open reaches neither
  Claude Code nor Antigravity. Status now comes from `ingress::status_for`, and
  two parity tests hold the line.
- **A guard that would not have failed before the fix is decoration.** Every
  parity and regression test added here fails against the commit preceding it.
  That is the bar worth keeping.
- **Verifying on the platform you are on is not verifying.** M25.12 is
  `cfg(target_os = "macos")` throughout, and the local gate only ever compiled
  the macOS half — so the Linux branch reached CI having never been built. Five
  of six jobs passed; the one that failed was the only one that had looked at it.
  `x86_64-unknown-linux-gnu` is installed but a cross-check still fails (`ring`
  wants `x86_64-linux-gnu-gcc`, and there is no C cross-toolchain here), so for
  `cfg`-gated code **CI is the only authority for the other platform** — plan for
  a round trip rather than expecting the local gate to be sufficient.
- **A grep for the type will not find an assertion on the status.** M11.9 changed
  which HTTP status an error maps to, and two tests asserting `503` were
  invisible to every search for `ModelUnavailable` — one of them in
  `panday-platform`'s smoke suite, which runs in the **integration lane, not the
  local gate**, and would have gone red on CI after the local run said green.
  Changing a mapping means auditing what asserts on the *output*, not only what
  matches on the *input*.
- **Half a measurement is not a measurement.** M25.3 kept
  `x-ratelimit-remaining-*` and not the matching `-limit-*`, which cannot make a
  percentage; and a 429 only proved the Codex token authenticates once a control
  request showed a bad token returns 401 instead. Same error twice: a number
  without its denominator, and a result without its control.

**One question carries no milestone number**, written into the spec rather than
only here: under `most_remaining`, a credential known to be at 2% is still
preferred over an unmeasured one, because ordering ranks only what it measures
and the threshold — not the ordering — handles emptiness (docs/25 M25.8).

*(The other one is closed. `ModelUnavailable` conflating an exhausted chain with
a model this deployment cannot serve became **M11.9**: 503 and 404 respectively.)*

---

## 4. Known problems

### 4.1 CI on `main` is green

The T1 hook 10ms flake on `c300fb1` (run `32330143170`) was a **timing flake, not a regression**:
docs/16 gives a wasm hook a 10ms wall-clock budget, and asserting behaviour against that while a
shared runner executes 900 other tests measures the runner. Fixed in `5fdf3f7` by splitting
behaviour (2s) from budget tests, with retries on the budget pair. `main` has been green on `ci`
since (see §2 for the current HEAD; this paragraph used to name `32fbbf2` and went stale within a
day, so it no longer names one).

### 4.2 SSH works. Start the agent first.

**This section used to say SSH was broken and to push over HTTPS. That was true
the day it was written and is not true now.** The keys were always fine; the
incident killed the *agent*, and `id_codeitlikemiley` is passphrase-protected, so
without an agent every push failed with `Permission denied (publickey)` — which
reads like a key problem and is not one.

The passphrase is already in the macOS keychain. One command:

```sh
ssh-add --apple-load-keychain     # loads id_codeitlikemiley, no prompt
ssh -T git@github.com             # → "Hi codeitlikemiley!"
```

Then `git push` and `git fetch` work normally against
`git@github.com:codeitlikemiley/panday.git`. Verified 2026-08-22.

Make it survive a reboot by adding to `~/.ssh/config` under `Host github.com`:

```
  AddKeysToAgent yes
  UseKeychain yes
```

**Do not switch `gh` to `hexuria` after pushing.** The old ritual here ended with
`gh auth switch --user hexuria`, which is pointless on this repo: **hexuria has
read access and no write access** (`Permission to codeitlikemiley/panday.git
denied to hexuria`). Switching back only guarantees the next `gh pr` call fails
with a permissions error that looks mysterious. Push and manage PRs as
`codeitlikemiley` and leave it there.

`~/.ssh/id_codeitlikemiley` is a 4096-bit RSA key from 2019. GitHub still accepts
RSA with SHA-2 so it is not urgent, but every other key on this machine is
ed25519 — rotating it is reasonable housekeeping whenever the user wants.

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

**Assert on your own rows, never on a global count or a bounded global listing.** That database
is shared by every test binary *and* it survives across runs, so both go stale in ways that get
worse over time:

- A **counter** from something that drains or scans the whole table belongs to whichever
  concurrent test won the race, not to you. Three billing tests asserted `report.applied >= 1`
  and a different one failed on each run — the event *had* applied, by someone else's drain. The
  `>= 1` looks like it tolerates sharing; it only tolerates other tests adding *more*.
- A **bounded listing** ordered for an operator is not a lookup. `billing::stuck(pool, 100)`
  orders most-attempted-first, so a row created a second ago sorts to the tail behind a few
  hundred permanently-stuck events from earlier runs. That test grew likelier to fail every time
  anyone ran the lane, and would eventually have failed permanently.

Scope by a fresh account (most tests), or by a schema when the table is deliberately unscoped —
`crates/panday-platform/tests/pg_vault.rs` does the latter, because the credential vault is
operator-global and so has no `account_id` to scope by.

**Run it twice before believing it.** A single green pass says nothing about order dependence,
and this lane is not in the local gate.

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

**A previous version of this section said "nothing is laptop-buildable any
more". That was wrong**, and it was wrong because it repeated the blocked-list
framing below instead of checking the tree. Two surveys on 2026-08-23 found the
opposite: several "blocked" milestones have substantial laptop-buildable slices
that had never been separated out. Do not invent rates or p95s to make the list
look shorter — but do not assume a milestone is blocked because this file once
said so.

0. **Merge PR #31** (M25.11 + the billing test fix). Green on all six jobs, still a draft because
   `gh pr ready` / `gh pr merge` were refused by the permission classifier. That closes docs/25
   at 12/12.

   It also confirmed the lesson above: this file called M25.11 blocked on "a Postgres" and the
   integration lane had been running one the whole time.

1. **M22.5 — the air gap is reproducible on this laptop.** `docker run --network none`, install
   the kit inside it, assert it completes and the verification command works on loopback only.
   Then pack a real small GGUF (`models/` has never been exercised) and write the per-version
   support-boundary doc docs/22 asks for. Verify the test *can fail* — edit `install.sh` to fetch
   something and confirm it goes red — before trusting it green. Leave the shape-3 compose/helm
   bundle out of `KIT_LAYOUT`; widening v1 scope is a judgement, not an omission.

2. **Phase 1 dogfood (the builder, not an agent).** Use the agent on a real repo under `dev` and
   decide whether you reach for it the next day. The fixture loop already passed live.
3. **A host (M22.2 leftover / M22.3).** The deploy workflow is written and guarded on
   `STAGING_DEPLOY_HOST`; set that secret plus `STAGING_SSH_KEY`, `STAGING_SMOKE_URL`,
   `STAGING_DATABASE_URL` and it runs, with the smoke suite gating the deploy. Do not rent a VM
   with the user's money unasked.
4. **KVM (M14.5–14.6, M22.4).** Two nodes, then measure fold latency. Do not invent p95s.
5. **Air-gap on real metal (M22.5's last mile).** Once the container test above is green, one
   install on a genuinely disconnected machine following only `INSTALL.md`. The container proves
   the installer needs no network; it does not prove the README is followable by a stranger.
6. **Stripe (M17.4 leftover / Phase 3 exit).** Live key, then the HTTP client and signature check.
   Do not add a Stripe crate without a key the user already set.
7. **Training (M19.3, then M19.5 / M19.7).** Labelled corpus that is *not* the 50 route-bench
   prompts; GPU-hours; docs/19 go/no-go before GRPO spend. The `Classifier` trait and shadow
   harness are the slot Model 1 drops into.
8. **If growing agent-bench back toward 50 tasks**, take them from mined traffic (M19.4), not from
   invention — and re-read §1.1 first. The twelve tasks removed were the destructive classes; they
   are not coming back.
