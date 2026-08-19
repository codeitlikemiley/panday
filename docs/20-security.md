# 20 — Security & Threat Model

Ordered by expected-loss, not by fashion. Each threat names its mitigations
and where they live. The platform's security posture in one sentence: **treat
every model output as attacker-influenced input, every plugin as hostile
code, and every tenant as adversarial to every other.**

## T1 — Prompt injection → tool misuse (the #1 threat, permanently)

Tool results, fetched web pages, repo files, and MCP outputs are untrusted
input that the model *will* sometimes obey.

- **Origin tagging**: every context block carries provenance
  (user | tool:{name} | web:{domain} | plugin:{id}); the system prompt and
  permission engine treat non-user origins as untrusted.
- **Permission gates are the real defense** (13): state-changing tools
  (`write`, `bash` mutating cmds, egress, `irreversible`) require profile
  allowance or Ask; injected instructions can't silently escalate past a
  human-visible gate.
- **Egress allowlists** (14): exfiltration needs a network path; deny by
  default, log every allowed request. Secrets never enter context (see T4).
- **pre_tool hooks** run deterministic filters (e.g., block `curl … | sh`,
  block writes outside workspace) — cheap, testable, model-free.
- Injection canaries in `agent-bench`: adversarial fixtures (poisoned READMEs,
  hostile tool output) with pass/fail gates per release.

## T2 — Sandbox escape (cloud multi-tenant)

- T3 microVMs for strangers' code — hardware virtualization boundary; jailer,
  cgroups, minimal /proc, no shared kernel with other tenants (14).
- The escape suite runs in CI; isolation regressions are broken builds.
- Defense in depth: even inside a VM, egress goes through the logged proxy;
  workspace volumes are per-session; snapshots are tenant-keyed.
- Patch posture: Firecracker/wasmtime/kernel updates are a weekly chore with
  an owner, not an event.

## T3 — Plugin supply chain

- Signing (ed25519) + registry tiers (16); `verified` requires review.
- WASM tools/hooks run in T1 with capability-scoped WIT worlds — a plugin
  literally cannot import what it wasn't granted; MCP stdio servers run under
  T2 policy with their declared net allowlist.
- Permissions are consent-at-install + Ask-at-first-use for MCP tools.
- No plugin code ever runs in the platform's own process. None.

## T4 — Secret exfiltration

- Secrets live in a vault table (or OS keychain locally), injected into
  sandbox env **only** when a tool's manifest declares the need and the
  permission engine approves; never rendered into model context.
- Redaction hooks scrub known secret patterns from tool output before the
  reducer (belt) and the gateway can run tenant DLP rules (suspenders).
- Egress proxy blocks requests whose bodies match active secret values
  (cheap, surprisingly effective).

## T5 — Cross-tenant data leakage

- Every query is tenant-scoped by construction: `account_id` in every table,
  enforced via sqlx query review + a CI lint that rejects unscoped queries on
  tenant tables; artifact store keys are prefixed and signed per tenant.
- Caches: exact-cache keys include tenant; semantic cache (if ever enabled)
  is per-tenant only. Provider prompt caches are inherently per-request-key —
  verified per adapter.
- Logs/traces scrub content by default (21); content-bearing debug modes are
  per-tenant opt-in with TTL.

## T6 — Billing abuse & fraud

- Free tier: no T3, no frontier pool, hard concurrency caps, disposable-email
  and device heuristics (17). Trial-abuse is a documented 2026 plague; assume
  it from day one.
- Idempotent ledger writes; rate limits per key AND per account AND per IP;
  spend anomaly alerts (10x hourly baseline → soft-lock + notify).
- API keys: hashed at rest, scoped, instantly revocable, never logged.

## T7 — Model/output safety

- We are infrastructure with a product on top: usage policy + provider-side
  safety inherited for provider pools; local models get our system-prompt
  guardrails and the same permission engine (safety-relevant *actions* are
  gated even when generation isn't).
- Abuse reporting hooks in the platform API; per-key kill switch.

## T8 — Availability & supply-chain of ourselves

- `cargo deny` (advisories/licenses) in CI; lockfile discipline; release
  binaries built in CI from tagged commits, signed, SBOM attached.
- Backups: PG PITR + object-store versioning; restore is *drilled* quarterly
  (a backup you haven't restored is a hope, not a backup).
- Incident runbook + status page from phase 3 (people pay us; act like it).

## Compliance trajectory (pragmatic)

Phase 3: SOC2-shaped controls without the audit (access logs, change review,
backup drills, vendor list). Phase 4+: SOC2 Type I→II when enterprise deals
demand; the event-sourced architecture makes evidence collection cheap —
the audit trail *is* the product's data model (ADR-002).

## Milestones

- **M20.1** Escape suite v1 (T2) in CI; injection canary fixtures in agent-bench. ✅ *(shipped: the T2 escape suites landed with M14.2 and run in CI on Linux and macOS; canaries are `panday_harness::canary` + `crates/panday-harness/tests/injection_canaries.rs`. **Deferred half**: embedding them in agent-bench, which does not exist until M19.6.)*

  **Every canary test scripts a model that fully complies.** That is the design. A suite where the
  model refuses measures the model's current disposition; a suite where the model obeys measures
  the layers that hold when it does not. What is claimed is narrow and true: an injection that
  completely convinces the model still cannot push, delete or exfiltrate without a human decision.
  What is not claimed is that the agent resists injection — it does not, and neither does anything
  else.

  **The suite found two real defects on its first run.**

  - **`read_only` asked instead of refusing.** An injected `git push --force` in a read-only
    session produced an "allow?" prompt, because "irreversible always asks" was checked before the
    profile default. Wrong shape: the profile has already answered the question, and putting it to
    a human anyway hands the injection a second chance with a tired reader. `read_only` now denies
    before the irreversible rule runs.
  - **`chmod -R 777 /` passed the filter pack.** Every destructive-root pattern was a *delete*, so
    a command that destroys nothing — and leaves every credential on the machine world-readable —
    walked through. It is worse than a delete, because there is nothing to restore.

  **Sixteen payloads across four goals**: run a command, read a secret, exfiltrate, and talk the
  agent out of asking at all. A corpus that is all "ignore previous instructions" measures one
  trick; the ones that matter are the plausible ones — a README that says to run the setup script,
  a polite request at the end of a code review, a JSON tool result claiming permission was granted.
- **M20.2** Origin tagging + pre_tool filter pack; secrets vault + env-injection policy. ✅ *(shipped: `panday_types::model::Origin` + `ContextBuilder`'s markers and `PROVENANCE_RULE`, `panday_harness::filters`, `panday_harness::secrets`; suite in `crates/panday-harness/tests/injection_defence.rs`.)*

  **Origin tagging.** `Origin` is additive and optional on `ToolOutput`/`Artifact`
  (docs/03 §Versioning), decoded in one place from the tool's registered name —
  docs/16 mounts MCP tools as `mcp:{server}:{tool}` precisely so provenance rides
  in the string every layer already carries. `is_trusted()` names the two trusted
  origins rather than excluding the untrusted ones: the untrusted list grows (MCP
  came after plugins, web after both) and a negative check would silently trust each
  addition. An **untagged** block is rendered `[origin: untagged]`, because a bare
  block reads as trusted and the one place provenance is missing is exactly where an
  attacker wants it missing.

  The marker is added during *assembly*, not when the event is written — the log
  records what happened, and a marker is a rendering decision — and it is
  deterministic, so the cached prefix stays byte-identical (ADR-008). The rule that
  gives markers meaning lives in the **stable band**: a safety rule that arrives
  after the untrusted content it governs is one the attacker got to speak first.

  **The filter pack is not a boundary, and says so.** A determined command evades any
  pattern list — `$(printf '\143url')` is `curl` — so the suite includes the
  evasions that get through, asserted as passing, with a note to update this bullet
  if the pack ever gets smarter. What these rules catch is the *unobfuscated* shape
  of an attack, which is what injected instructions overwhelmingly look like because
  the attacker is writing for a model, not a parser. T2's `--unshare-net` is what
  actually blocks egress.

  The pack's other design constraint is false positives: a pack that vetoes real work
  gets switched off, and then none of it helps. So `rm -rf ./target` passes while
  `rm -rf /` does not, `echo $TOKEN` passes (the scrub covers the output) while
  `curl -d "t=$TOKEN" …` does not, and the workspace rule applies to writes only —
  vetoing reads outside the workspace would break every `cargo` invocation that
  touches `~/.cargo`. A veto names the rule that fired. Filters scan string *values*
  at any depth rather than the serialized JSON, so a key named `curl` is not a
  finding. It holds under `unleashed`, which is the point of a model-free layer.

  A found bug: a `~`-relative write path is not absolute, so joining it to the
  workspace placed `workspace/~/.ssh/authorized_keys` "inside" and the rule passed
  it. Home-relative paths are now refused rather than guessed at.

  **Secrets.** Three conditions, all required: declared in the manifest (the
  `secrets:` grant consented to at install), approved by the permission engine, and
  present in the vault. An approval without a declaration is refused too — otherwise
  a gate answer could widen a manifest nobody re-consented to. `MemoryVault::from_env`
  takes explicit names only; a vault that swept the environment would hand a tool
  every credential the developer happened to have exported. A vault lists *names*,
  never values, so an audit log cannot become a leak.

  **The scrub runs before the reducer**, and that ordering is the whole trick:
  reduction is lossy and its spilled artifacts are content-addressed, so a secret
  that survives into the reducer is a secret in the artifact store forever. Scrubbing
  first also means it never reaches the event log, and therefore never a replay —
  which the test asserts on the serialized log rather than on the tool's return
  value. It scrubs known *values*, not patterns: a pattern list guesses at what a
  credential looks like and misses the one that does not match, while the vault knows
  exactly which strings are secret (pattern-based DLP belongs at the gateway, where a
  false positive costs a redaction rather than a broken turn).
- **M20.3** Tenant-scoping CI lint; cache key audit; trace scrubbing defaults. ✅ *(shipped in three places: `panday_platform::tenancy` + `crates/panday-platform/tests/tenant_scoping.rs` (the lint), `CacheKey` at M11.6 (the cache-key audit's finding, built in), `crates/panday-sdk/tests/scrub_audit.rs` at M21.5 (scrubbing defaults).)*

  **The lint is armed before the first query exists.** Postgres is M3.5, so today the
  scan finds no SQL — which is exactly when this is worth writing. The first unscoped
  query is the one written while someone is debugging something else, and by the time
  there are fifty queries a lint becomes a migration project instead of a guardrail.

  **The rule is coarse on purpose.** A statement touching a tenant table must *mention*
  `account_id`; whether the predicate is correct is a code review's job. `AND
  account_id = $1` in the wrong place is a review finding, no `account_id` at all is a
  data breach, and only the second is decidable by a lint. Comments are stripped first,
  because a commented-out predicate is precisely how a scoped query becomes unscoped
  during a debugging session. DDL gets the stronger check — a `CREATE TABLE` for a
  tenant table must *declare* the column, since a query lint cannot help if there is
  nothing to filter on.

  Tenant tables and global tables are both explicit lists: a table not on the tenant
  list is asserting it holds no tenant data, which is a claim someone makes on purpose
  rather than by omission. Matching is on word boundaries, so `accounts` does not fire
  on `service_accounts_audit` — a lint that cries wolf gets deleted.

  **One exemption exists**, added at M18.3 when the lint fired on the first SQL written
  after it was armed. The finding was correct and the rule was not: `panday local`'s SQLite
  database has no accounts in it (docs/18: "no account needed at all"), so a per-file
  `tenant-scoping: single-tenant — <reason>` marker exempts it. It must name a reason, must
  sit in the first 40 lines so it reads as a property of the module rather than an excuse
  next to a query, and a test counts the files claiming it — an exemption nobody can find
  is one nobody reviews, and one that needs no reason is one that spreads.

  Test code is skipped, and has to be: a lint's own fixtures are examples of the thing
  it forbids. Which leaves the failure mode that "no SQL in the repo" and "the scanner
  is broken" look identical, so a planted-violation test builds a temp tree with one
  `.sql` file and one Rust string literal and requires both walkers to find them. It
  plants outside the repo because writing into it would race the scan test running
  concurrently — which the first draft did, and failed.
- **M20.4** Abuse guardrails live (velocity, anomaly alerts, kill switches); backup restore drill #1 documented. ✅ *(shipped: `panday_platform::abuse`, migration `0008_abuse_controls.sql`, `panday-platform suspend|unsuspend|watch`, `scripts/backup-drill.sh` / `just drill`.)*

  **Three mechanisms, in increasing order of the certainty they need.** Velocity checks are
  advisory: thirty accounts from one source in an hour is a signal, not a verdict, and `watch`
  *reports* — a heuristic wired to an irreversible action will eventually be wrong about a real
  customer on their busiest day. Disposable-email detection is advisory too, and deliberately a
  checked-in **list** rather than a cleverness: a regex that guesses at throwaway domains catches a
  university and misses `mailinator`, and a list is something a customer who writes in can be shown.
  Sub-addressing (`user+tag@`) is explicitly not suspicious — it is how careful people track who
  leaked their address, and punishing it annoys exactly the customers worth keeping.

  **The kill switch is certain, immediate and reversible.** A timestamp and a reason on the account,
  read in the same query as the API key — so it bites on the very next request with nothing to
  invalidate and no window where a killed account still works. Not a delete: a deleted account
  cannot be investigated and cannot be reinstated. Both the suspension and the reinstatement are
  written to an append-only `admin_actions` table in the same transaction as their effect, because
  "who turned it off" and "who turned it back on" are the first two questions an incident review
  asks — and an admin action that leaves no trace is indistinguishable from an intrusion.

  **Drill #1, run and recorded.** `scripts/backup-drill.sh` dumps the database, restores it into a
  *different* one, and compares the numbers that matter: account count, ledger entry count, ledger
  sum, and whether the restored `balances` summary still agrees with the restored entries. A drill
  that only checks `pg_restore`'s exit code proves that `pg_restore` exited zero. It refuses to
  overwrite a restore target that already holds tables — a drill that can destroy the thing it is
  rehearsing for is not a drill — and it runs its client tools in a container matching the server's
  major version, because a laptop's Homebrew `pg_dump` is routinely older than the database and
  refuses outright.

  **Result of drill #1 (2026-08-19, dev stack, `postgres:17-alpine`):** 5 accounts, 52 ledger
  entries, balance −1,275,000 micro-credits dumped and restored identically; the derived `balances`
  table agreed with the restored entries. The consistency check was then verified to *fail* by
  deleting one entry from the restored copy — it reported one account whose balance disagreed. A
  check that has never failed is a check nobody has tested.
- **M20.5** SBOM + signed releases; dependency-update cadence with an owner. ✅ *(shipped: `cargo xtask sbom`, the checked-in `sbom.cdx.json`, the `sign` job in `.github/workflows/release.yml`, `.github/dependabot.yml`.)*

  **The SBOM is checked in, not only released.** A document produced at release time answers "what
  did we ship"; a reviewer needs "what are we about to ship". `cargo xtask sbom --check` runs in CI,
  so a dependency change without a regenerated SBOM fails — an SBOM that disagrees with the lockfile
  describes a build nobody is making, and is worse than none because it is believed.

  **Reproducible by construction.** No timestamp, no serial number, components sorted. Two runs on
  the same tree produce byte-identical files, so a diff in a PR is a real dependency change rather
  than noise — which is the only condition under which anyone reads one.

  **Hand-rolled from `cargo metadata --locked`**, like the Prometheus exposition and for the same
  reasons: the output is a documented format, the input is one command's JSON, and the alternative
  is a tool fetched from the network on every CI run. Licences are emitted as SPDX *expressions*,
  not ids — `MIT OR Apache-2.0` is not a licence id, and a consumer that reads it as one records a
  licence that does not exist.

  **Signing is keyless.** Sigstore/OIDC: there is no private key to store, rotate, or leak, the
  signature is bound to the release workflow's identity in a public transparency log, and a fork
  cannot produce one that verifies against this repository. One signature over one `SHA256SUMS`
  manifest, because signing N artifacts means verifying N signatures and nobody does that. The
  release body carries the two commands that verify it — an unverifiable release is an unsigned one
  with extra steps.

  **The cadence has a name on it.** Dependabot, weekly (a batch a person reviews; a daily stream is
  the same as no updates, with noise), minor and patch grouped into one PR so a security fix is not
  queued behind twenty-nine cosmetic ones, reviewer `@codeitlikemiley`. GitHub Actions are updated
  on the same schedule: a compromised action runs with this workflow's token, and this workflow can
  sign artifacts. `cargo deny` and the full suite gate every one of them.
