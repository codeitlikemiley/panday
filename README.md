# Panday — a Rust AI platform, built from first principles

> **Panday** is Tagalog for *blacksmith*. The name is settled, not a placeholder
> (`docs/00-vision.md` §Naming): crates are prefixed `panday-`, the CLI binary is
> `panday`, and API keys are `pnd_live_` / `pnd_test_`.

This repository is the specification *and* the implementation of a full AI
infrastructure platform: an agent harness, an LLM gateway with a model router, a tiered
sandbox, a plugin system (skills + MCP + ACP), a token-economy layer, a
subscription platform with metered billing, an offline/local tier, and a
path to training your own task models.

**Two things live here:**

| Path | What it is |
|---|---|
| `docs/` | The documentation set — 21 specs, ADRs, threat model, roadmap. The source of truth. Renders with `mdbook serve docs`. |
| `crates/` | The implementation. `cargo nextest run --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` are green on every commit; `docs/` is updated in the same commit whenever the code diverges from a spec. |

## Try it

Nothing below needs an account, a server, or a network connection.

```bash
# Rust 1.95, pinned in rust-toolchain.toml.
cargo build --release

# 1. One-shot chat through your own gateway: routed by policy, metered per call.
#    Grok CLI login (~/.grok/auth.json) enables `xai` — no API key:
./target/release/panday chat -m xai/grok-4.6 "why is this test failing?"
#    Or ANTHROPIC_API_KEY / Claude Code login, or PANDAY_BASE_URL for any
#    OpenAI-compatible server.
#    Outbound secrets (Grok/Claude/Codex OAuth, API keys) go in the vault with
#    `panday creds` — pipe the token, or `--from-grok` / `--from-claude` / `--from-codex`.
#    Never argv. See docs/25.
#    ./target/release/panday creds add --from-grok --label laptop
#    ./target/release/panday creds list

# 2. Offline: a local model, a jailed workspace, nothing leaving the machine.
#    Needs an OpenAI-compatible server on loopback (llama-server, mistral.rs, …).
#    A remote base URL is refused rather than honoured.
./target/release/panday-local --workspace . "read src/lib.rs and explain it"

# 3. Your editor, over ACP. Point Zed / JetBrains / nvim at this command:
./target/release/panday acp --workspace .

# 4. Read any session back, exactly as the CLI showed it — including one that
#    crashed halfway. The log is the state (ADR-002).
./target/release/panday replay .panday/session.jsonl --costs
```

**Point an agent at the local gateway** (`docs/11` §Pointing agents). After
`panday-gateway` is listening (this laptop: `127.0.0.1:8088`):

```bash
# OpenAI-shaped clients (Grok Build custom model, Codex, curl, Python SDK)
curl -sS http://127.0.0.1:8088/v1/chat/completions \
  -H 'Authorization: Bearer unused' -H 'Content-Type: application/json' \
  -d '{"model":"xai/grok-4.6","messages":[{"role":"user","content":"pong"}],"max_tokens":64}'

# Claude Code — Anthropic Messages. --bare or it talks to Anthropic directly.
# Do not put /v1 on the URL.
export ANTHROPIC_BASE_URL=http://127.0.0.1:8088 ANTHROPIC_API_KEY=unused
claude --bare --print "reply with the single word pong"

# Antigravity CLI (`agy`) — Gemini generateContent, not Gemini CLI.
# Install the CLI only. agy does not read .env. Do not put /v1beta on the URL.
# ~/.gemini/antigravity-cli/settings.json must have {"modelProvider":"gemini"}
export GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8088 GEMINI_API_KEY=unused
agy --print "reply with the single word pong"
```

**Several Grok logins or API keys** (`docs/25`). After the gateway is up, open
`http://127.0.0.1:8088/accounts` — import Grok CLI, paste another `auth.json`,
add OpenAI / xAI / Anthropic / Gemini keys, pick **failover** or **round-robin**.
Or at boot:

```bash
# extra Grok CLI auth.json copies (colon-separated)
export PANDAY_GROK_AUTH=/path/to/other-auth.json
# extra API keys (comma-separated)
export PANDAY_OPENAI_API_KEYS=sk-test-aaaa,sk-test-bbbb
export PANDAY_ROTATE=round_robin   # or failover (default)
```

**The hosted shape, on a laptop.** `just dev` brings up Postgres and MinIO, migrates,
and prints an API key with the `curl` that uses it — accounts, per-key rate limiting,
the ledger and the route audit all wired (`docs/22-deployment.md`).

```bash
just dev        # stack up, migrated, one API key printed once
just serve      # the platform on the host, against that database
just check      # fmt, clippy, the whole test suite
```

**Skills port unmodified.** Drop a directory containing a `SKILL.md`: frontmatter
keys other runtimes use (`allowed-tools`, `license`, nested `metadata`) are
ignored rather than rejected, and `references/` loads on demand
(`docs/16-plugins.md` §Skills).

**MCP servers mount as tools.** They run as child processes with a cleared
environment, appear as `mcp:{server}:{tool}`, and ask before every call until you
grant them per tool (`docs/16` §MCP host).

## Status

**Every milestone that does not need hardware or a third party is shipped.**
Phases 0–2's numbered work is complete (Phase 1's *exit* still wants you to use
the agent on a real repo; Phase 2's still wants Zed and a GGUF). Phase 3 is
built except Stripe and a host; phase 4 except T3 on KVM; phase 5's
infrastructure is in place and its models are not. Live traffic on this laptop
uses Grok CLI OAuth (`xai/grok-4.6` against `api.x.ai`) as the *outbound* hop.
Inbound, Claude Code / Grok Build / `agy` point at `panday-gateway` — that is
not routing through OpenCodex or LiteLLM.

`docs/23-roadmap.md` carries the honest sequencing and a per-phase status. Every
shipped milestone is marked ✅ in its own spec together with what was learned
building it — including the bugs, of which the useful ones were found by tests
that could have passed. What is not built says so, and names what it needs:
Firecracker wants KVM, the trained models want GPUs and a corpus, "a stranger
pays" wants a Stripe key, and the air-gap kit wants an air-gapped machine to be
installed on. Each sits behind a trait with an `#[ignore]`d test rather than a
fake, because a fake would produce numbers instead of evidence.

## How to use this repo

The docs are written to be **executable by agents**. Each component spec ends
with numbered milestones that carry acceptance criteria — each milestone is
sized to be one focused session for a coding agent (or a week of your own
evenings). Hand a spec + the workspace to an agent, point at a milestone, and
review the diff.

Read in this order:

1. `docs/00-vision.md` — what we are building and in what order
2. `docs/01-architecture.md` — the system, on one page of diagrams
3. `docs/04-decisions.md` — the ADRs; why each contested choice went the way it did
4. `docs/23-roadmap.md` — the honest sequencing
5. The component spec for whatever you are building this week

## The short pitch

One binary-per-service Rust platform where:

- Every model call goes through **your** gateway (`panday-gateway`) — cost
  attribution, caching, failover, and the router live there.
- The agent loop (`panday-harness`) is an event-sourced state machine — every
  session is an append-only log you can replay, resume, branch, and audit.
- Tool output passes through a **reducer** before it touches context — the
  token bill is a first-class engineering target, measured against real
  prompt-cache economics, not vibes.
- Untrusted execution is **tiered**: in-process pure tools → WASM plugins →
  namespaced processes → Firecracker microVMs.
- Skills are markdown, tools are MCP, editors connect over ACP — we adopt the
  open protocols and compete on the runtime.
- Subscriptions and the API are the same metering pipeline; offline mode is a
  first-class deployment target, not an afterthought.
- Model training starts with evals, proceeds by LoRA on rented GPUs, and every
  artifact exports to GGUF so the local tier benefits too.
