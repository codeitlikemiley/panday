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

- **M20.1** Escape suite v1 (T2) in CI; injection canary fixtures in agent-bench.
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
- **M20.3** Tenant-scoping CI lint; cache key audit; trace scrubbing defaults. *(Two thirds landed elsewhere: the cache-key audit's finding is built into `CacheKey` at M11.6 — `account_id` is part of the key by construction, with a test that one tenant's prompt cannot serve another's response — and trace-scrubbing defaults are audited statically and at runtime by M21.5. What remains here is the CI lint that catches a *new* query or cache key built without a tenant scope.)*
- **M20.4** Abuse guardrails live (velocity, anomaly alerts, kill switches); backup restore drill #1 documented.
- **M20.5** SBOM + signed releases; dependency-update cadence with an owner.
