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
- **M18.3** SQLite event store passes the same harness suite as PG (one test matrix, two stores). ✅ *(shipped: `panday_local::sqlite::SqliteStore` + `panday_harness::store_conformance`; matrix in `crates/panday-local/tests/store_matrix.rs`.)*

  **The matrix is one function, not one suite per store.** `store_conformance::run` holds
  every invariant the harness depends on, and each store is a call site: `MemoryStore`,
  `SqliteStore`, `JsonlStore` today, Postgres by adding one entry when M3.5 brings it.
  Writing them once matters because the invariants are not local — "gapless `seq`" is what
  makes `after_seq` a complete sync mechanism (docs/03), and a store that got it subtly
  wrong would fail far away, as a client that silently stops receiving events.

  **It immediately found a bug in a store that was already shipped.** `JsonlStore` (M21.3)
  checked gaplessness on *read* and not on *append*, so it would happily write a log it
  would later refuse to read — by which point the event that should have been there is
  gone. It now checks on append, and refuses a second session in one file (one file, one
  session) rather than interleaving seqs.

  **The schema is the invariant.** `PRIMARY KEY (session_id, seq)` makes a repeated `seq`
  an error rather than a silent overwrite, which is ADR-002's single-writer rule enforced
  by the database instead of by hope. The gapless check runs in the same transaction as the
  insert — outside it, two tasks could both read `max=1`, both write 2, and the loser would
  get a unique-violation instead of the clear error. Pool size is one, for the same reason.

  Events are stored as **JSON text, not columns**: docs/03 requires unknown kinds to
  round-trip verbatim, which a column layout cannot do, and a migration per event kind
  would make "additive fields are always ok" false in practice. A test stores a
  `cache_warmed` event from a newer version and reads back both its tag and its payload.

  `synchronous = FULL` with WAL, because "the event is in the log" has to mean it is on the
  disk rather than in the page cache (docs/13 §persist-before-proceed) — the crash this
  store exists to survive is a laptop lid closing.

  `export_jsonl` bridges to `panday replay`, which takes a log file: a session in a
  database is not one, and the person debugging is usually not the person whose laptop it
  happened on.

  **The M20.3 lint fired on the first SQL written after it was armed**, exactly as
  intended — and the finding was a true negative: an offline database has no accounts to
  scope by (docs/18: "no account needed at all"). So the lint gained a per-file
  `tenant-scoping: single-tenant — <reason>` exemption that must name a reason, sits in the
  first 40 lines, and is counted by a test so a second claim has to be noticed. One file
  claims it today.
- **M18.4** Capability profiles wired: same prompt on cloud vs local produces adapted system prompt + toolset (snapshot-tested). ✅ *(shipped: `panday_types::CapabilityProfile`, `ContextBuilder::with_capabilities`, on by default in `panday local`; suites in `crates/panday-harness/tests/capability_profiles.rs` and `crates/panday-local/tests/offline_turn.rs`.)*

  **"Measured by the eval suite — not vibes" is enforced by a field.** Every profile carries
  `Provenance`, and a `Declared` one makes the prompt say **(estimated)**. The numbers
  shipped today are declared: M19.2 measures them. A model told "your JSON reliability is
  0.7" by a number nobody measured is being lied to precisely, and the hedge is what keeps
  the honesty in the product rather than in a comment.

  **The adaptations are thresholds on the profile, not a "local" flag.** A large local model
  needs none of them and a small hosted one needs all of them, so `wants_minimal_tools`
  reads tool reliability, `wants_aggressive_reduction` reads the window, and
  `wants_expectation_notice` reads both. Three things change, all in the **stable** band so
  they ride in the cached prefix rather than being injected later (ADR-008): the prompt gains
  a constraints section, the tool set shrinks to reads-plus-`bash`, and the window becomes
  the profile's *usable* context — compaction at 70% of a number the model cannot use fires
  too late, and too late means the turn fails instead of degrading.

  **The constraints section is absent when there is nothing to say.** A frontier profile
  produces no section at all; telling a 200k-context model its context size is a sentence
  paid for on every turn to say nothing, and a section full of non-constraints teaches the
  reader to skip it — which is how the real constraints get missed. Same reason the lines are
  facts about the model ("you cannot see images") rather than advice ("be careful with
  JSON"): a fact is checkable and advice is not actionable.

  The dropped tools are the ones where being wrong costs work — `write_file`, `edit_file`, a
  plugin tool — while a bad `read_file` costs a turn. And no profile means no claims:
  assuming frontier capabilities when nobody said is what produces a local model confidently
  promising to read an image.
- **M18.5** mistral.rs as alternate runner behind a flag.
- **M18.6** Sync: offline sessions appear in cloud account after reconnect; ledger reconciles.
- **M18.7** Air-gap kit: one tarball (binary + catalog + models) installs on a machine with no internet; documented for enterprise.
