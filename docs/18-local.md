# 18 — panday-local: the offline tier

One binary, zero egress, same architecture (ADR-010, vision principle
"offline is a deployment target"). This is simultaneously: the free tier's
foundation, the enterprise air-gap story, the fallback when providers are
down, and the forcing function that keeps every interface honest.

## Composition

`panday local` (in `panday-local`) wires, in one process:

- **harness** with SQLite (or single-file redb) event store
- **gateway-lite**: the same gateway lib, adapters restricted to `local`,
  ledger writing to local store (still metered — usage syncs later)
- **router** with the file collapsed to `local-only` pools (12)
- **model runner supervisor**: spawns and health-checks a local inference
  server; v1 target **llama-server** (llama.cpp) as default,
  **mistral.rs** as the Rust-native alternative (both OpenAI-compat, both
  GGUF; the gateway's `local` adapter doesn't care which)
- **panday-cli** attaches to it exactly as it would to the cloud — the client
  cannot tell (this is tested, not aspired to)

## Model management

```
panday models list                 # installed + catalog
panday models pull qwen3.5-4b-q4   # resolves to a pinned GGUF artifact
panday models verify               # sha256 + license check
panday models rm …
```

- Catalog = a signed JSON index we publish (model name → GGUF URL, sha256,
  license, context length, RAM estimate, capability profile). Enterprise
  mirrors host the same index internally; `pull` honors a mirror URL.
- Default catalog picks (Aug 2026, revisit quarterly): Qwen3.5 4B/9B (Apache),
  gpt-oss-20b (Apache) for bigger boxes, Qwen3-Coder-Next-80B-A3B for
  serious local rigs. Licenses are surfaced, not hidden — the catalog refuses
  to list anything we can't redistribute.
- Our own tuned artifacts (19) enter the same catalog — the summarizer and
  (later) coding specialist ship as GGUF like any other entry.

## Degraded-capability honesty

Local models are worse. Pretending otherwise ruins trust in the product.
The router returns a `CapabilityProfile` (max context, JSON reliability,
tool-call reliability, no vision) and the harness **adapts**: system prompt
states the constraints, tool schemas shrink to the minimal set, subagent
fan-out drops, the reducer switches to `aggressive`, and ambitious tasks get
a "this will be slower/rougher locally" notice event the UI renders. Profiles
are per-model entries in the catalog, measured by the eval suite — not vibes.

## Sync

Offline sessions accumulate in the local event store. On reconnect (if the
user links an account — optional, not required): logs push up (append-only
merge is trivial — sessions are single-writer, ADR-002), local ledger entries
reconcile into the account ledger (usage.model entries with
`provider_cost_micros: 0` — they still count against fair-use metering for
free tiers). Conflict-free by construction; no CRDT machinery needed.

## Licensing

Free tier: no account needed at all — `panday local` with your own models is
genuinely free (this is the top of the funnel; don't poison it).
Enterprise: entitlement token file (17) unlocks seats/features; validated
offline via ed25519 pubkey baked into the binary.

## Milestones

- **M18.1** `panday local` boots harness+gateway-lite+CLI against an already-running llama-server; end-to-end turn with tools, no network. ✅ *(shipped: `panday_local::Local`, the `panday-local` binary, `crates/panday-router/policy/local.yaml`; suite in `crates/panday-local/tests/offline_turn.rs`, with the live llama-server leg `#[ignore]`d.)*

  **Zero egress is enforced, not intended.** `Local::boot` refuses a non-loopback base
  URL. Everything else about the offline tier points the same way — one adapter, one pool,
  a policy file with nowhere else to go — but all of that is *configuration*, and
  configuration is what gets changed by someone in a hurry. The loopback check is the one
  part that cannot be reconfigured into egress by editing YAML. It matches on the host
  rather than resolving it, because resolving would itself be a network call and a name
  that resolves to loopback today can resolve elsewhere tomorrow (`localhost.evil.example`
  is in the test).

  **gateway-lite is the same gateway with one adapter**, not a smaller reimplementation.
  docs/18 calls this tier "the forcing function that keeps every interface honest", and a
  second gateway would be the first thing to drift — so metering, routing, the cache and
  the breakers are the cloud's code paths, exercised locally.

  **One file, one session.** The store is `JsonlStore` (single-file append-only; SQLite
  parity is M18.3), and a second boot on the same log *adopts the session already in it*.
  The first draft minted a fresh session id, which started at seq 1 in a file that already
  had one — and the store rejected it as a single-writer violation, correctly. Reopening
  the file is the whole recovery story (ADR-002): fold, finish what was in flight if it is
  replay-safe, and carry on with gapless seqs.

  **What CI proves and what it does not.** The model server in the suite is a fake
  OpenAI-compatible endpoint on loopback, because CI has no GGUF and no GPU. The
  composition, the wire dialect, the T2 jail, the tools, the log and the rendering are all
  real — docs/18's own claim is that the local adapter "doesn't care which" server it is.
  The literal wording of this milestone ("against an already-running llama-server") is an
  `#[ignore]`d test that anyone with one running can execute:
  `cargo test -p panday-local -- --ignored`.

  Output goes through the same `replay::Renderer` the CLI and the hosted client use, so an
  offline session, a hosted session and a replay are one format — which is what makes
  "the client cannot tell" something you can check rather than assert. Usage is metered at
  real token counts and zero money: a free tier that reports nothing is a free tier nobody
  can reason about.
- **M18.2** Model supervisor: spawn/health/restart llama-server; `models pull/verify` with signed catalog.
- **M18.3** SQLite event store passes the same harness suite as PG (one test matrix, two stores).
- **M18.4** Capability profiles wired: same prompt on cloud vs local produces adapted system prompt + toolset (snapshot-tested).
- **M18.5** mistral.rs as alternate runner behind a flag.
- **M18.6** Sync: offline sessions appear in cloud account after reconnect; ledger reconciles.
- **M18.7** Air-gap kit: one tarball (binary + catalog + models) installs on a machine with no internet; documented for enterprise.
