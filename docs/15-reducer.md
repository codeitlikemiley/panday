# 15 — ferrum-reducer (the token economy)

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

- **M15.1** Trait ✅ + generic fallback (head/tail + error-float) + artifact spill + `expand_artifact` tool; fixtures for 5 output types.
- **M15.2** Structural: cargo + git + pytest/jest compressors with retention fixtures; measured ≥60% token cut on the fixture corpus at zero retention failures.
- **M15.3** File-read dedup/diff-awareness wired into read/grep tools.
- **M15.4** Dollar accounting with cache-state input; per-session savings report event; dashboard tile.
- **M15.5** Reduce-then-solve replay eval harness; nightly job + regression gate.
- **M15.6** Semantic tier behind budget gate (provider cheap model); swap-in point defined for our tuned summarizer.
