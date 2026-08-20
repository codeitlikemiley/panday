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

---

## 2. Where the project stands

**HEAD:** `c300fb1` — *M19.6: agent-bench, rebuilt so the jail does the containing*
**Remote:** `git@github.com:codeitlikemiley/panday.git` (public, user `codeitlikemiley`)

**89 milestones total: 83 shipped ✅, 3 partial, 3 not started.**

| Spec | Shipped |
|---|---|
| 02-workspace, 03-protocol, 10-sdk, 11-gateway, 12-router, 13-harness, 14-sandbox, 15-reducer, 16-plugins, 17-platform, 18-local, 20-security, 21-observability | **all** |
| 19-training | 4 / 7 |
| 22-deployment | 2 / 5 |

Phases 0–2 complete. Phase 3 (Money) complete except what needs Stripe and a host. Phase 4 (Scale
surfaces) complete except T3 on real hardware. Phase 5 (Own models): all infrastructure, no models.

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

Everything upstream of them exists: eval spine + scorecard artifact (M19.1), capability-profile
generator (M19.2), shadow-mode classifier comparison (M12.5), consent-first transcript mining
(M19.4), agent-bench as the RL environment (M19.6), signed model catalog for a tuned GGUF (M18.2).

---

## 3. In-flight, uncommitted work

`git status` is **not clean**. Two files are modified and *not* committed:

```
 M .config/nextest.toml
 M crates/panday-sandbox/tests/t1_hooks.rs
```

**What this is:** a fix for the red CI (see §4). `crates/panday-sandbox/tests/t1_hooks.rs` gained a
`behaviour_limits()` helper (2s wall clock) and the tests that assert *behaviour* now use it, while
the two tests whose subject *is* the 10ms budget keep `T1Limits::hook()`. `.config/nextest.toml`
gives those two tests retries (2 local, 3 in CI).

**Not verified.** It compiles as far as `cargo test -p panday-sandbox --test t1_hooks` was started,
but the run was interrupted. Next agent should: run that test binary, run the full suite, then commit.

---

## 4. Known problems

### 4.1 CI on `main` is RED

Run `32330143170` (`ci` workflow on `c300fb1`) failed: **2 of 912 tests**, both in
`crates/panday-sandbox/tests/t1_hooks.rs` — `a_hook_can_veto_a_tool_call` and
`a_hook_can_rewrite_arguments`, each with `Deadline(10ms)`.

It is a **timing flake, not a regression**: docs/16 gives a wasm hook a 10ms wall-clock budget, and
asserting that while a shared runner executes 900 other tests measures the runner. This has now
happened twice (the first time was fixed by arming the budget *after* instantiation). The
uncommitted work in §3 is the fix. The whole suite passes locally (922/922) and the **nightly job
passed** (run `32332735956`), including the isolation and agent-bench jobs.

### 4.2 SSH push is broken — use HTTPS

The ssh-agent socket was destroyed in the incident. The keys are intact
(`~/.ssh/id_codeitlikemiley`) but passphrase-protected, and the agent that held them is gone. Push
with:

```sh
git -c credential.helper='!gh auth git-credential' \
  push https://github.com/codeitlikemiley/panday.git HEAD:main
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
just bench    # json-bench against a running gateway (needs a model)
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

1. **Finish §3 and get CI green.** Run `cargo nextest run -E 'binary(t1_hooks)'`, then the full
   suite, then commit and push over HTTPS (§4.2).
2. **Nothing else is code-blocked.** Every remaining milestone needs hardware or data:
   - M22.3 → a host. The deploy workflow (`.github/workflows/deploy.yml`) is written and guarded on
     `STAGING_DEPLOY_HOST`; set that secret plus `STAGING_SSH_KEY`, `STAGING_SMOKE_URL`,
     `STAGING_DATABASE_URL` and it runs, with the smoke suite gating the deploy.
   - M22.4 → two KVM nodes, then measure the fold latency.
   - M22.5 → install the kit on an air-gapped machine following only `INSTALL.md`.
   - M19.3 → a labelled corpus and a few GPU-hours; the `Classifier` trait and shadow harness are
     the slot it drops into.
   - M19.5, M19.7 → training runs, weeks and dollars. docs/19 puts a go/no-go review before the GRPO
     spend deliberately.
3. **If growing agent-bench back toward 50 tasks**, take them from mined traffic (M19.4), not from
   invention — and re-read §1.1 first. The twelve tasks removed were the destructive classes; they
   are not coming back.
