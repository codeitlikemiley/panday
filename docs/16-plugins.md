# 16 — ferrum-plugins: skills, MCP, hooks, ACP

The extension story rides on open standards (ADR-005): **skills** are
markdown, **tools** are MCP, **editors** are ACP, and the only invention is
the packaging + trust wrapper around them.

## The package

```
my-plugin/
├── plugin.toml            # identity, version, entry points, permissions requested
├── skills/
│   └── deploy-check/SKILL.md
├── mcp/                   # zero or more MCP server definitions
│   └── github.toml        # transport: stdio cmd | http url; env requirements
├── tools/                 # zero or more WASM component tools (T1)
│   └── lint_fast.wasm
└── hooks/                 # WASM hooks (pre_tool, post_tool, …)
```

`plugin.toml` declares **requested capabilities** (mobile-app style):
`fs: workspace-ro`, `net: [api.github.com]`, `secrets: [GITHUB_TOKEN]`,
`hooks: [pre_tool]`. Install-time consent; the sandbox tiers enforce
(T1 WIT world for WASM, MCP servers run as T2 children with that policy).
Distribution: `.plugin` archive, ed25519-signed; registry tiers
`verified | community | unlisted`, with the marketplace being a phase-4
storefront over the same registry API.

## Skills

SKILL.md-compatible on purpose — the existing ecosystem should port with zero
edits: YAML frontmatter (`name`, `description`, trigger hints) + markdown
body + optional `references/` loaded on demand.

Runtime semantics (harness 13): the skills **index** (name + description,
~1-2 lines each) lives in the stable prefix; a skill's **body** loads into
`~stable` when triggered — explicitly (`/deploy-check`), by the model
(`load_skill` tool), or by the trigger classifier — and stays for the session
(unloading churns cache, ADR-008). Token budget per skill body (default 2k;
oversized skills get their tail spilled to artifacts with expand-on-demand).

## MCP host

Via `rmcp` (official SDK): stdio (child process under T2 policy) and
streamable HTTP transports; OAuth flows for the servers that need them.
Mounted MCP tools appear in the ToolRegistry as `mcp:{server}:{tool}` behind
the same `Tool` trait — the loop can't tell them from native tools; the
*permission engine* can (MCP tools default to `Ask` until granted).

Schema hygiene: MCP tool schemas can be enormous; the registry minifies
descriptions into the stable prefix and lazy-loads full schemas on first use
(the ToolSearch pattern) when a server exposes >N tools.

We also **serve** MCP: `ferrum mcp` exposes our native tools + a session's
context to other MCP clients — cheap interop, and it forces our tool layer to
stay spec-clean.

## Hooks (untrusted)

Plugin hooks are WASM components implementing `ferrum:plugin/hook` — same
lifecycle points as in-process hooks (13) with vetoes limited to `pre_tool`.
Fuel-metered, epoch-interrupted, 10ms default budget; a hook that exceeds it
is skipped and the event logged. Hooks see *redacted* views (no secrets in
args).

## ACP bridge

`ferrum acp` (in ferrum-cli, ADR-012) speaks Agent Client Protocol v1 over
stdio using the official `agent-client-protocol` crate. Mapping is mechanical:
session/new + prompt → `UserMessage`; AEP `AssistantDelta/ToolCall/ToolResult`
→ ACP session updates; `PermissionRequest` → ACP's permission flow. Thirteen
editors (Zed, JetBrains, VS Code, nvim, Emacs…) become clients for the cost
of one adapter — the best distribution-per-line-of-code in the plan.

## Milestones

- **M16.1** plugin.toml parse + capability model + signature verify; skill loader with frontmatter (types ✅ for manifest basics).
- **M16.2** Skills index/lazy-body in harness assembly; two real skills ported unmodified from the existing ecosystem.
- **M16.3** MCP client host (stdio under T2): mount a public MCP server, call its tool through the loop with Ask-gating.
- **M16.4** WASM tool + hook runtime (wasmtime, WIT world v1); fuel/epoch limits enforced in escape suite.
- **M16.5** ACP bridge: interactive session from Zed; permission round-trip works.
- **M16.6** Registry service (publish/fetch/verify) + `ferrum plugin install`; marketplace UI deferred to phase 4.
