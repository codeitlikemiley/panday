# 15 — panday-reducer (the token economy)

Every tool result passes through the reducer before it enters context. The
mandate is not "compress everything"; it is **minimize dollars while
preserving task-relevant information** — and those diverge in exactly the
ways the prior art exposed.

## What rtk got right, and what the benchmark taught

rtk (rtk-ai/rtk, Rust, Apache-2.0) proved per-command structural compression
works: 60-90% reduction on bash output via smart filtering of git/cargo/test
output. Then a JetBrains AI-team benchmark (blog post, Jul 2026) measured **~0% net
cost change** in real Claude Code sessions. Their analysis: rtk wraps CLI
*commands* (integrated via a PreToolUse hook that rewrites Bash invocations),
so per their measurements only a minority of tool-result characters ever
passed through it — Read/Grep results bypassed it entirely — and cached
re-reads dominated cost (cache reads price at ~0.1x, so shaving
already-cached tokens saves a dime per dollar of effort).

Both findings are design inputs (ADR-007):

1. **Coverage**: we sit at the harness boundary and reduce *every* tool
   result — Read, Grep, Bash, MCP tools, subagent results — not one channel.
2. **Cache-aware accounting**: savings are priced in dollars using the real
   multipliers (fresh input 1x, cache reads ~0.1x, Anthropic-style cache
   writes 1.25x–2x by TTL, no write premium on automatic-caching providers),
   and we never churn stable prefixes to "save" tokens.

## Where it sits

```
sandbox ExecStream ──▶ REDUCER ──▶ ToolResult { output: Reduced, raw_ref: ArtifactRef }
                          │
                          └──▶ artifact store (full raw, content-addressed)
```

The raw output is never lost — it spills to object storage; the context gets
the reduced form plus a handle. A follow-up tool (`expand_artifact(ref,
range)`) lets the model pull exact ranges when the digest wasn't enough —
**reduction is reversible on demand**, which is what makes aggressive
defaults safe.

## Strategy stack (applied in order, first sufficient wins)

```rust
pub trait Reducer: Send + Sync {
    fn reduce(&self, input: RawOutput, ctx: ReduceCtx) -> Reduced; // ctx: tool, args, task, budget
}
```

1. **Structural compressors** (per tool/command family — the rtk move):
   parse and re-emit canonical terse forms. v1 set, chosen by measured
   frequency in coding sessions: cargo build/test (errors+warnings with
   file:line, counts, elide the wall of green), git status/diff/log
   (porcelain-style digest; diffs keep hunks, drop context lines beyond ±2),
   test runners (failures verbatim, passes as counts), package managers
   (resolved/added/removed counts), ls/find/glob (tree summary + counts,
   full list spilled).
2. **File-read dedup & diff-awareness**: re-reading a file the context
   already holds returns "unchanged since seq N" or a hunk-diff against the
   held version. (This is the Read/Grep coverage rtk lacked — and it must
   cooperate with the cache: the *original* read stays verbatim in the
   rolling window; the dedup applies to the new result only.)
3. **Generic fallback**: head+tail windows with `[… 1,204 lines elided —
   expand_artifact(ref, 120..180) …]` markers; error-pattern extraction
   (lines matching error/warn/fail/panic float into the kept window).
4. **Repeat suppression**: hash match with a prior result → "identical to
   result at seq N".
5. **Semantic summarization** (opt-in, budget-gated): a `cheap`-pool model
   (later: our own tuned 2-4B summarizer — Model 2 in 19.5, M19.5) digests what structure
   can't. This is the only strategy that costs tokens to save tokens — the
   accounting below decides when it's worth it.

## The accounting model (what "worth it" means)

For each candidate reduction:

```
value = (tokens_removed × expected_reads × marginal_price_per_token)
      − (summarization_cost if any)
      − (information_risk penalty)
```

- `marginal_price` uses the session's actual model + cache state: tokens that
  would land in the rolling window get re-sent every turn (≈ turns_remaining
  × price, cache-read-discounted after first send).
- `information_risk` is strategy-dependent: structural compressors carry
  retention tests (below) → near zero; generic elision on *error* output →
  high penalty (keep more).
- Metrics emitted per event: `tokens_raw`, `tokens_kept`, `est_dollars_saved`
  — the dashboard number is dollars, never percent (ADR-007). An `rtk`-style
  90% that saves $0.002 is reported as $0.002.

## Retention tests (the quality gate)

Every structural compressor ships with fixtures: recorded real outputs +
assertions that the *task-relevant* facts survive (the failing test's name,
the compiler error's file:line, the conflicted path). A compressor PR without
retention fixtures is rejected. The nightly "reduce-then-solve" eval (19)
replays recorded sessions with reduction on/off and diffs task success —
regression there blocks release, because a reducer that loses the plot is
negative value at any compression ratio.

## Config

Per-tool overrides, per-session profile (`aggressive` for background
subagents, `conservative` for interactive), global kill-switch
(`reducer: off`) for debugging "did compression eat it?" — one flag, not an
argument.

## Milestones

- **M15.1** Trait ✅ + generic fallback (head/tail + error-float) + artifact spill + `expand_artifact` tool; fixtures for 5 output types. ✅ *(shipped: `panday_reducer::artifact` (content-addressed store, sha256 per docs/03) and `::spill` (`SpillingReducer`); `panday_harness::ExpandArtifact`; corpus in `crates/panday-reducer/tests/fixtures/`.)*

  **Error-float now carries context.** The seeded version floated only the
  line matching an error marker, which kept `error[E0308]: mismatched types`
  and dropped the very next line, `--> gateway.rs:142:23`. A compiler error's
  location, a Python traceback's assertion and a test runner's
  `left`/`right` all sit *below* the matched line, so single-line floating
  keeps the word "error" and loses the fact. `error_context_lines` (default
  3) fixes it; the `cargo_build_error` fixture is the regression test.

  **Spill is conditional.** A result is only spilled when it was actually
  reduced *and* is over `min_spill_bytes` — the model never calls
  `expand_artifact` on output it can already see in full, so a blob there is
  pure cost.

  **Recorded baseline for M15.2**, generic fallback over the corpus:

  | fixture | raw → kept | cut |
  |---|---|---|
  | cargo test (1 fail in 260) | 1967 → 481 | 76% |
  | cargo build (E0308) | 822 → 529 | 36% |
  | git status (120 paths) | 1091 → 561 | 49% |
  | pytest (1 fail in 340) | 6235 → 1241 | 80% |
  | large clean file read | 9346 → 705 | 92% |
  | **overall** | **19461 → 3517** | **82%** |

  M15.2's structural compressors must beat these *without* failing any
  retention assertion. Note `cargo build` is the weakest at 36% — diagnostics
  are mostly signal, which is the right outcome, and the structural compressor
  should improve it by dropping the dependency-compile preamble rather than by
  trimming the error.
- **M15.2** Structural: cargo + git + pytest/jest compressors with retention fixtures; measured ≥60% token cut on the fixture corpus at zero retention failures. ✅ *(shipped: `panday_reducer::structural` — `StructuralReducer`, shapes `CargoTest` / `CargoBuild` / `Pytest` / `GitStatus`.)*

  **Measured, against the M15.1 baseline, zero retention failures:**

  | fixture | generic | structural |
  |---|---|---|
  | cargo test | 76% | **96%** |
  | cargo build | 36% | **89%** |
  | git status | 49% | **94%** |
  | pytest | 80% | **98%** |
  | large file read | 92% | 92% *(no structure; stays generic)* |
  | **corpus** | **82%** | **95%** |

  `cargo build` improved most, exactly as predicted at M15.1 — the win came
  from dropping the dependency-compile preamble, not from trimming the
  diagnostic.

  **Detection is by content, not by command**, because the harness sees
  `bash` for everything and the command line is not in `ReduceCtx`. That is
  also more honest: `make test` shelling out to cargo still gets the cargo
  treatment.

  **A detection false-positive is the worst failure this layer has**, and the
  fixtures caught one: the first `git status` heuristic accepted two leading
  spaces as a status field, so *every indented text file* matched — a Rust
  source file was detected as a status listing and rewritten by that
  compressor, destroying the code. Under-compressing is a missed saving;
  misdetecting produces output that is confidently wrong. The status field now
  requires a real status letter, and indented Rust, YAML and Markdown are
  regression-tested as `Unknown`.

  A compressor whose output is *larger* than its input falls back to generic
  rather than shipping a "reduction" that costs tokens.
- **M15.3** File-read dedup/diff-awareness wired into read/grep tools. ✅ *(shipped: `panday_reducer::reads` — `ReadLedger`, `ReadOutcome`, `hunk_diff`; wired into `read_file`, invalidated by `write_file`/`edit_file`.)*

  This is the channel the JetBrains benchmark showed rtk never covered. Agents
  re-read the same files constantly, and over a long session those re-reads
  dominate the bill.

  **Only the new result shrinks.** The original read stays verbatim in the
  rolling window, exactly as the spec's parenthesis requires — rewriting it to
  a stub would churn a cached prefix and re-price it at 1x instead of ~0.1x, a
  "saving" that costs money (ADR-008).

  **A write invalidates the held copy.** Reporting "unchanged" about content the
  agent itself just replaced is a *wrong answer*, not a missed saving, so both
  `write_file` and `edit_file` clear the belief and both are tested.

  A changed re-read diffs against the **most recently sent** version, not the
  original, so the model is never shown changes it has already seen. Diffs keep
  ±2 lines of context and summarise the rest; a diff of two very large files
  reports that it is too large rather than grinding through a quadratic LCS
  table and stalling the turn.
- **M15.4** Dollar accounting with cache-state input; per-session savings report event; dashboard tile. ✅ *(shipped: `panday_reducer::accounting` — `Pricing`, `CacheState`, `value_of`, `SessionSavings`.)*

  Savings are computed in **micro-dollars with integer arithmetic**, not floats:
  a figure summed thousands of times a session should not accumulate binary
  rounding error, and the ledger (docs/17) is integer-based for the same reason.

  The tests are mostly about *not* being impressed by ratios, because that is the
  failure this milestone exists to prevent:

  - **A 90% reduction on a local model reports \$0.** There is no marginal token
    cost, so the ratio is real and the saving is nothing — docs/15's own example,
    made executable.
  - **Eliding error output cheaply reports a loss.** `net_micros` is signed, so
    "a reducer that eats the failing test's name is negative-value at any
    ratio" is something the accounting can actually *say*.
  - Retention-tested structural compressors carry ~zero information risk;
    generic elision does not get that credit, and over error output it is
    penalised ~20×.
  - The same reduction is worth more over more turns, and ~10× more when the
    rolling window is *not* cached — which is precisely the discount that made
    rtk's raw ratios worth ~nothing.

  `dashboard_line()` puts dollars first and the ratio in parentheses, because a
  ratio in the lead position is how a \$0.002 saving gets celebrated as a 90%
  win.
- **M15.5** Reduce-then-solve replay eval harness; nightly job + regression gate. ✅ *(shipped: `panday_harness::eval`, `crates/panday-harness/tests/reduce_then_solve.rs`, `cargo xtask reduce-bench`, `.github/workflows/nightly.yml`.)*

  **Current scorecard** (the five recorded outputs the retention fixtures use):
  100% of solvable tasks still solved, mean reduction 93.7%, zero regressions —
  `cargo_test_v1` 95.8%, `cargo_build_v1` 88.6%, `pytest_v1` 98.3%,
  `git_status_v1` 93.6%, `generic_headtail_v1` 92.5%.

  **Why this is not the retention suite again.** Those fixtures assert a fact
  survived the *compressor*. This asserts the task stayed solvable through the
  whole *loop* — assembly, spilling, compaction — and a fact can survive the first
  and be lost in the second. The solver in the loop is mechanical rather than an
  LLM: a real model would make the eval non-deterministic and put its own judgement
  under test instead of the reducer's. It succeeds only if every required fact is in
  the context the loop assembled, which is a lower bound on a real model's needs and
  the part that can be gated in CI.

  **The gate is asymmetric on purpose.** A regression is "solvable with reduction
  off, unsolvable with it on". A scenario that fails both ways is a broken scenario,
  and failing the build on it would teach everyone to ignore this gate — so a second
  test asserts every scenario is solvable with reduction off, or a broken corpus
  would silently exempt itself.

  **The harness is tested by making it fail.** A `Vandal` reducer that keeps two
  lines scores 99.0% reduction and breaks exactly the three failure diagnoses; the
  two remaining scenarios survive because their fact happens to sit in the first
  line. That is the real lesson about head-keeping strategies — they look fine on a
  file read and destroy a test failure, because the interesting part of a diagnostic
  is at the end. An eval that cannot fail says nothing, and this one's whole claim
  is that it notices when an impressive ratio costs task success (ADR-007).

  `PassthroughReducer` landed with this as the control arm, and it is also docs/15
  §Config's `reducer: off` kill-switch. It is a `Reducer` and not an
  `Option<Reducer>` so the loop is byte-identical in both arms — otherwise the
  comparison measures the loop, and the debugging flag would change two things at
  once.
- **M15.6** Semantic tier behind budget gate (provider cheap model); swap-in point defined for our tuned summarizer. ✅ *(shipped: `panday_reducer::semantic`, `panday_harness::CheapPoolSummarizer`, `SessionActor::with_pricing`/`with_semantic_tier`.)*

  **A found bug, and the reason this milestone had to fix it first.**
  `ReduceCtx.price_per_token_micros` was micro-dollars per *token*: $3/mtok is
  0.003 of those, which in `u64` is zero. Every accounting decision keyed on it
  therefore priced every real model as free — silently, because the arithmetic
  still ran. It is now `price_per_mtok_micros`, the same unit as
  `Pricing::input_per_mtok_micros`. This is exactly the ADR-007 failure the
  accounting exists to prevent, hiding inside the accounting.

  **Layer 5 is not a `Reducer`.** `reduce` is a synchronous pure function, right
  for layers 1–4; layer 5 makes a model call, and squeezing it into that trait
  would mean blocking on the harness's runtime mid-turn. So the one strategy that
  needs IO lives at its own seam and the loop awaits it.

  **The gate is money, not size.** Every other layer is free, so "is it big" is
  reason enough to run. This one spends tokens, so it runs only when
  `tokens_removed × expected_reads × marginal_price − summarizer_cost − risk`
  clears a floor. Consequences the tests pin down: a **local model is never
  summarized** (marginal price zero — spending cloud tokens to save free ones is
  ADR-007's mistake with an extra API call); an **unpriced session** is not
  assumed free either, and declines; **error output is held to a higher bar** than
  prose at identical token counts, because a summary is a lossy re-write and
  docs/15 puts a high information-risk penalty there — the same numbers that
  justify summarizing progress output must not justify summarizing a stack trace.

  Three failure modes it refuses: a summarizer that is **down** costs the turn
  nothing but the structural output it already had (this is the only layer with a
  network dependency, and a context pipeline that broke when a summarizer was
  unreachable would be worse than not having the layer); a summary that came back
  **larger** is discarded, because providers do echo the input when a prompt
  confuses them; an **empty** summary is an error rather than a 100% reduction,
  which would be the most expensive possible bug here.

  `tokens_raw` keeps the *original* raw count through layer 5, so the dashboard
  measures the whole pipeline's saving rather than crediting the summarizer with
  the structural compressors' work.

  **The swap-in point is the `Summarizer` trait**: M19.5's tuned 2–4B model
  implements it and nothing above changes. `CheapPoolSummarizer` declares
  `task: summarize` and leaves the model as `auto` so docs/12's policy picks the
  pool — hard-coding a model here would bypass the one component whose job is
  choosing one. Whether a swap was an improvement is decided by M15.5's
  reduce-then-solve scorecard, which compares task success: a cheaper summarizer
  that loses facts fails the gate instead of looking like a win.
