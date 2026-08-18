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
- **M20.2** Origin tagging + pre_tool filter pack; secrets vault + env-injection policy.
- **M20.3** Tenant-scoping CI lint; cache key audit; trace scrubbing defaults.
- **M20.4** Abuse guardrails live (velocity, anomaly alerts, kill switches); backup restore drill #1 documented.
- **M20.5** SBOM + signed releases; dependency-update cadence with an owner.
