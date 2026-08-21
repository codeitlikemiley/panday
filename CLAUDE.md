# CLAUDE.md — working agreement for this repo

Panday is a Rust AI platform (agent harness, LLM gateway + router, tiered
sandbox, plugins, metering, offline tier). **Panday** is Tagalog for
*blacksmith*; the name is settled, not a placeholder (see `docs/00-vision.md`
§Naming). Crates are prefixed `panday-`, the CLI binary is `panday`, and API
keys are `pnd_live_` / `pnd_test_`.

## 1. `docs/` is the source of truth

The specs in `docs/` are not documentation *of* the code — they are the design
the code implements. They were written first and they win by default.

- If the code must diverge from a spec, **update the spec in the same commit**
  and state why in the commit message. A spec that disagrees with shipped code
  is a bug in the spec (`docs/23-roadmap.md` §Standing weekly rhythm).
- Never silently work around a spec. Either follow it, or change it on the
  record.
- If a spec is ambiguous or self-contradictory, surface it to the user rather
  than picking a reading and burying the choice in code.

## 2. Reading order

Read these in order before writing any code in a new session:

1. `handover.md` — current HEAD, what is blocked, the incident rules
2. `docs/README.md` — what the repo is, how it is meant to be used
3. `docs/00-vision.md` — what we are building and in what order
4. `docs/01-architecture.md` — the system in four diagrams and ten sentences
5. `docs/04-decisions.md` — the ADRs; why each contested choice went that way
6. `docs/23-roadmap.md` — the honest phase sequencing
7. Then the component spec for whatever you are building
   (`docs/02`, `docs/03`, `docs/1x-*.md`)

`docs/SUMMARY.md` is the mdBook index if you need the full map.

## 3. Workflow: one milestone per session/commit

Every component spec (`docs/02`, `docs/03`, `docs/1x-*.md`) ends with a
**Milestones** section — numbered items (M11.1, M3.2, …) each carrying
acceptance criteria. Each is sized for one focused session.

- Work **exactly one milestone per session and per commit**. Do not batch
  milestones into one commit, and do not half-land two.
- Before **every** commit, both of these must be green:
  ```
  cargo test --workspace          # or: cargo nextest run --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ```
  Also `cargo fmt --all --check` — CI gates on it (`.github/workflows/ci.yml`).
- The commit message names the milestone, e.g. `M11.1: openai_compat adapter`.
- A milestone is done when its stated acceptance criteria hold — not when the
  code merely compiles.

## 4. Never start phase N+1 to avoid finishing phase N

This is the one rule in `docs/23-roadmap.md`, and the pre-mortem there lists
"building phase 4–6 infrastructure with phase-1 users" as failure mode #1.

Each phase has a falsifiable **exit criterion**. Do not begin work belonging to
a later phase because it is more interesting than what remains in the current
one. If the current phase looks finished, check its exit criterion literally
before moving on.

**A phase is not done when its milestones are done — it is done when its exit
criterion holds.** These diverge: every Phase 0 milestone was green while all
three binaries were still `println!` stubs, because the wiring that satisfies
the exit had no milestone number. When you find such a gap, add the milestone
(as M0.1 was added to `docs/23`) rather than leaving the work invisible.

Also note the roadmap's phase lists are **selective**, not exhaustive — they
name representative milestones. Most of the ~100 milestones across the specs
appear in no phase at all, so "not in a phase list" does not mean "not needed".
`handover.md` is the current count and the blocked list — and it is a count that
moves, so recount rather than quoting either document from memory.

Related standing traps from the same pre-mortem, worth re-reading before any
design decision: the gateway is the engine room, not the product; no training
before evals; invent only AEP and conform everywhere else.

## 5. Dependency policy — ask first

`docs/02-workspace.md` carries the dependency table (tokio, axum, reqwest+rustls,
serde, thiserror/anyhow, sqlx, uuid, `time` (not chrono), tracing, schemars,
rmcp, agent-client-protocol, wasmtime, ratatui).

- **Adding anything outside that table requires asking the user first.** This
  includes transitive-heavy or network-touching crates, and it includes dev-
  dependencies.
- Versions are pinned in `[workspace.dependencies]`; member crates inherit with
  `foo.workspace = true`. Do not pin a version inside a member crate.
- `panday-types` keeps near-zero dependencies (serde, serde_json, thiserror,
  uuid, time). Everything depends on it; it depends on nothing of ours.
- `cargo deny check` gates the license/advisory graph — keep it green.

## 6. Architectural invariants that are easy to break by accident

- **Libraries take traits, binaries do the wiring.** `panday-harness` accepts
  `ModelClient`/`Sandbox`/`EventSink`; HTTP and Postgres live in the binaries.
  This is what makes `panday local` possible.
- **No `unsafe` outside `panday-sandbox`.**
- **AEP is append-only with gapless `seq`.** Deltas are ephemeral; only folded
  messages persist. State is a fold over the log — if a state cannot be
  rebuilt from events, the missing event is the bug (`docs/03`).
- **Protocol changes are golden-file diffs.** Fixtures live in
  `crates/panday-types/tests/fixtures/`; a changed fixture is a reviewed
  protocol change, and `proto/` schemas are regenerated with
  `cargo xtask schemas`.
- **Context layout is stable→volatile** (ADR-008). Nothing may inject into the
  stable cached prefix mid-session.
- **Usage cache counts are subsets of `input_tokens`** — adapters normalize at
  the boundary (see the CONVENTION note on `Usage` in
  `crates/panday-types/src/model.rs`).

## 7. Repo conventions

- Workspace: `crates/panday-*`, one binary per service, `lib.rs` + thin
  `main.rs` so every service is testable in-process.
- Toolchain is pinned in `rust-toolchain.toml` (currently 1.95); ratchet
  monthly, never mid-crunch.
- `cargo xtask` is the codegen pattern — not `build.rs` cleverness.
- Tests must not need network or Docker. Anything that does is `#[ignore]`d and
  belongs to the integration lane (`docs/02` §CI shape).
