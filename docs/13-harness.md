# 13 — panday-harness

The heart. A session actor that runs the agent loop as an explicit state
machine over the event log. Everything else in the platform exists to feed
this loop or observe it.

Prior art acknowledged and mined: Claude Agent SDK (hooks, subagents,
permission modes, skills — the pattern we're building an owned instance of),
OpenAI Agents SDK (handoffs, guardrails), rig's sans-IO AgentRun. What none
of them give us: our event sourcing, our reducer integration, our metering,
our sandbox tiers. That's the 60% we write.

## The actor

One tokio task per live session, owning:

```rust
pub struct SessionActor {
    state: SessionState,          // fold of the log (rebuildable, ADR-002)
    log: Box<dyn EventStore>,     // PG in cloud, SQLite/file in local
    model: Box<dyn ModelClient>,  // the SDK pointed at the gateway
    tools: ToolRegistry,          // native + plugin + MCP-mounted
    permissions: PermissionEngine,
    reducer: Box<dyn Reducer>,
    sandbox: Box<dyn Sandbox>,
    budget: TurnBudget,           // steps, tokens, wall-clock, spend
    subs: Vec<EventSink>,         // connected clients (WS), ACP bridge
}
```

Inbox messages: `UserInput`, `PermissionDecision`, `Cancel`, `Fork`,
`ClientAttach(after_seq)`. Crash recovery = reload fold of log, resume at
last stable state. The actor is the *only* writer to its log — no locks, no
races, gapless `seq` for free.

## The turn state machine

```
Idle
 └─ UserInput → Assembling
Assembling: context = layout(state)          # cache-aligned, see below
 └─→ Streaming: model.chat(ctx) → fold deltas, emit AssistantDelta
      ├─ text only → Finalize(turn)
      └─ tool_calls[] → Gating
Gating: for each call → PermissionEngine
      ├─ Allow → queue        ├─ Ask → emit PermissionRequest, park call
      └─ Deny → synthesize ToolResult(error, "denied by policy")
 └─→ Executing: run queued calls (parallel where tools declare independence)
        each: sandbox.exec → raw → reducer.reduce → ToolResult event
 └─→ back to Assembling (loop)          # observations folded in
Stops: MaxSteps | BudgetExceeded | Cancelled | ModelStop(end_turn)
 └─→ Finalize: TurnFinished{reason, usage, cost} → Idle
```

Hard invariants, each enforced in code and tested:

- **Bounded**: `max_steps` (default 20), wall-clock, and spend ceilings are
  checked at every transition; exceeding one is a *normal* stop reason, never
  a panic.
- **Persist-before-proceed**: an event is fsync'd to the log before the state
  machine advances past it. On a crash between tool exec and result-write,
  resume **replays idempotent calls and refuses irreversible ones** —
  surfacing the refusal with the call's args, since the tool may or may not
  have run. `side_effects: irreversible` additionally forces `Ask` regardless
  of profile, so a human saw every irreversible dispatch; but Ask is consent,
  not replay-safety — the refuse-on-replay rule is what prevents double
  execution.
- **Cancellation is a first-class transition** from every state; in-flight
  sandbox execs get SIGTERM→grace→SIGKILL; the model stream is dropped
  (provider bills what streamed — record it).

## Context assembly (the cache-aligned layout, ADR-008)

```
[ stable  ]  system prompt · tool schemas · skills INDEX (names+descriptions)
[ ~stable ]  loaded skill bodies · memory notes · compaction summaries
[ rolling ]  transcript window (recent turns verbatim)
[ hot     ]  current turn: user msg · this turn's tool results
```

- Anthropic: cache breakpoints at the stable/~stable and ~stable/rolling
  boundaries. OpenAI: the same ordering exploits automatic prefix caching.
- **Compaction**: when projected tokens > threshold (default 70% of model
  window), summarize the oldest rolling span into a `~stable` note via a
  `cheap`-pool model, emit `Compaction{span, refs, before, after}`. The full
  text stays in the log/artifacts — compaction is a *view* optimization,
  never data loss.
- Skills load lazily: index always present; bodies enter `~stable` on
  trigger (16), and *stay* for the session (re-injecting/removing churns
  cache).

## Tools

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;         // name, description, schema, annotations
    fn requirements(&self) -> ToolReq;  // sandbox tier, side_effects, independence
    async fn call(&self, ctx: ToolCtx, args: Json) -> ToolOutcome;
}
```

Native set (phase 1): `read_file`, `write_file`, `edit_file`, `bash`,
`grep`, `glob`, `web_fetch` (through egress proxy), `spawn_subagent`. Plugin
tools mount via MCP (16) under the same trait — the loop cannot tell them
apart, which is the point.

**Registry discipline**: tools are enumerable, versioned, and their schemas
are part of the stable prefix — adding/removing tools mid-session is a cache
break and therefore a deliberate, logged act.

## Permissions

```rust
// The engine's verdict for a call:
pub enum Gate { Allow, Deny, Ask }
// The recorded decision on an Ask (by user or policy):
pub enum PermDecision { Allow, AllowRemember, Deny }
pub struct PermissionEngine { profile: Profile, overrides: Vec<Rule>, remembered: Vec<Grant> }
```

Profiles: `read_only`, `dev` (read/edit/test allowed; git push, package
publish, network egress → Ask), `unleashed` (local only, still gates
`irreversible`).

> **`side_effects` serves two masters, and `bash` exposes the seam** (found at
> M13.2). The field drives both *consent* (which profiles prompt) and *replay
> safety* (what resume may re-run). For `bash` these disagree: the profile
> table above says `dev` allows running tests, so the tool cannot be
> `Irreversible` — that would force Ask in every profile, `unleashed`
> included, making an unattended run impossible by construction. But replaying
> an arbitrary shell command after a crash *is* unsafe, which is exactly what
> `Irreversible` is for.
>
> Resolution: `bash` declares `Idempotent` (the common case — running a build
> or a test suite), and genuinely dangerous invocations are gated by the
> argument patterns this section already describes (`bash(rm -rf*) → Ask`),
> which is per-command rather than per-tool. **M13.5 must therefore not treat
> `Idempotent` as "safe to re-run on resume" for `bash`**; replay safety needs
> its own signal, decided there. Rules match (tool, args-pattern) — e.g. `bash(rm -rf*) → Ask`
even in unleashed. Decisions can be remembered per-session or per-project
("always allow cargo test here") → stored as `PermissionDecision` events, so
grants are auditable and replayable like everything else.

## Subagents

`spawn_subagent(brief, toolset, budget)` creates a child session with its own
log, a *restricted* tool registry, a fraction of the parent budget, and no
access to parent context beyond the brief. Parent receives
`SubagentFinished{result_ref}` — the reduced result, not the child's
transcript. Depth ≤ 2, fan-out capped by plan. This is how "be comprehensive"
scales without one context window eating the bill.

## Hooks

Sync extension points, run in-process (trusted, ours) or as plugin WASM
(untrusted, 16): `pre_turn`, `pre_model`, `post_model`, `pre_tool(call)`
(may rewrite args or veto — this is where policy DLP and the reducer's
command-rewrites hang), `post_tool(result)`, `on_compaction`, `on_stop`.
Hook misbehavior (timeout, panic) is contained: log, skip, continue.

## Milestones

- **M13.1** State machine with fake ModelClient: scripted multi-tool loop, golden event log. ✅ *(shipped: `panday_harness::actor` — `SessionActor`, `MemoryStore`, `EventSink`; `panday_harness::testing` — `ScriptedClient`, `EchoTool`, `render_log`; golden at `crates/panday-harness/tests/fixtures/multi_tool_loop.jsonl`.)*

  `TurnOutcome::AwaitingPermission` is deliberately **not** a `StopReason`: a
  turn parked on a gate is mid-flight, not finished, and collapsing the two
  would make a paused turn indistinguishable from a completed one in the log.
  The Ask flow that resumes it is M13.3.

  Context assembly here is a plain transcript. The cache-aligned
  stable→volatile layout and compaction are M13.4 — building them now would
  be guessing at a design that milestone exists to measure.
- **M13.2** Real model via gateway + native read/grep/bash tools + T2 sandbox: fixes a real failing test in a fixture repo, unattended. ✅ *(shipped: `panday_harness::native` — `read_file`, `write_file`, `edit_file`, `grep`, `glob`, `bash`; fixture at `crates/panday-harness/tests/fixture_repo/`; proof in `tests/fix_a_failing_test.rs`.)*

  **What is proven, and what is not.** The loop repairs a genuinely broken
  crate — the fixture's `sum_to` is off by one and its test fails before the
  run — through the real native tools inside a real T2 jail, and the test
  asserts the repo is green afterwards by invoking `cargo test` on it
  *outside* the harness. It also asserts the fix is in the implementation and
  the test is still present, since deleting the test would also make `cargo
  test` pass.

  The model is scripted, per docs/02 ("the harness suite runs without network
  using the fake client"): the script chooses the *plan*, everything beneath
  it is real. Whether a **live** model chooses those steps is the remaining
  leg, kept as an `#[ignore]`d test that needs `ANTHROPIC_API_KEY`:

  ```text
  ANTHROPIC_API_KEY=… cargo test -p panday-harness --test fix_a_failing_test -- --ignored
  ```

  `web_fetch` (needs the egress proxy, M14.2) and `spawn_subagent` (M13.6) are
  deliberately absent rather than stubbed — a tool the model can call but that
  cannot work is worse than one it never sees.
- **M13.3** Permission engine + Ask flow over WS; cancellation kills a sleeping bash cleanly. ✅ *(shipped: `Rule` argument patterns in `panday_harness::permissions`; `SessionActor::decide`; `SessionActor::cancel` / `cancel_handle`.)*

  **Gate order is the security model**, and it is not the obvious one:

  1. explicit `deny` rules — nothing overrides an operator's "never";
  2. `Irreversible` forces Ask;
  3. remaining `ask`/`allow` rules;
  4. remembered grants;
  5. profile default.

  Denies run *before* both the irreversible-Ask and remembered grants. If a
  remembered "always allow bash" outranked a deny rule, it would silently
  authorise `bash(rm -rf /)` later in the same session; and if Ask outranked
  deny, an operator's hard limit would degrade into a dialog someone can click
  through. Rules match a canonical, lowercased, key-sorted rendering of the
  arguments, so they cannot be evaded by case or JSON key order.

  **The Ask flow is tested at `decide`, not over a WebSocket** — the WS
  endpoint is M3.3 and the transport carries the same events either way. The
  decision is committed *before* the call it authorises, so a crash between
  consent and execution leaves the consent on the record.

  **Cancellation is real, not cooperative bookkeeping.** Dropping an
  `ExecStream` closes its channel, which the sandbox's reader task treats as
  "the caller is gone" and kills the child. The test starts a genuinely
  sleeping `bash`, cancels it, and then proves the process died by checking a
  marker file the command would have written had it survived.
- **M13.4** Cache-aligned assembly + compaction; measured: ≥70% cache-read ratio on a 30-turn session replay.
- **M13.5** Crash-kill during Executing → resume replays correctly (idempotent) and refuses (irreversible) — both proven by tests.
- **M13.6** Subagents with budget split; parallel independent tools.
- **M13.7** Hook engine with in-process hooks; pre_tool veto demonstrated.

Acceptance stance: the harness suite runs **without network** using the fake
client; every invariant above has a named test.
