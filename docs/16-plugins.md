# 16 — panday-plugins: skills, MCP, hooks, ACP

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

We also **serve** MCP: `panday mcp` exposes our native tools + a session's
context to other MCP clients — cheap interop, and it forces our tool layer to
stay spec-clean.

## Hooks (untrusted)

Plugin hooks are WASM components implementing `panday:plugin/hook` — same
lifecycle points as in-process hooks (13) with vetoes limited to `pre_tool`.
Fuel-metered, epoch-interrupted, 10ms default budget; a hook that exceeds it
is skipped and the event logged. Hooks see *redacted* views (no secrets in
args).

## ACP bridge

`panday acp` (in panday-cli, ADR-012) speaks Agent Client Protocol v1 over
stdio using the official `agent-client-protocol` crate. Mapping is mechanical:
session/new + prompt → `UserMessage`; AEP `AssistantDelta/ToolCall/ToolResult`
→ ACP session updates; `PermissionRequest` → ACP's permission flow. Thirteen
editors (Zed, JetBrains, VS Code, nvim, Emacs…) become clients for the cost
of one adapter — the best distribution-per-line-of-code in the plan.

## Milestones

- **M16.1** plugin.toml parse + capability model + signature verify; skill loader with frontmatter. ✅ *(shipped: `PluginManifest::parse`, `FsCapability`, `panday_plugins::signature`, `panday_plugins::skill`.)*

  **The manifest is a consent document, and the validation follows from that.**
  A wildcard domain is *refused, not expanded*: `*.example.com` reads as a
  narrow grant and is in fact a grant to anything anyone can register under it,
  which nobody can meaningfully consent to. `fs` became a typed enum because the
  seed's free string accepted `workspac-ro` and then read as "no access
  requested" — a typo that made a plugin look *less* dangerous than it is. A
  name that could traverse the filesystem, or carries control characters, is
  refused. Unknown keys are refused too: writing `hooks = [...]` after
  `[capabilities]` makes it a key of that table, and permissively that request
  is dropped and the plugin installs with no hooks and no complaint.

  The consent prompt names every grant individually and echoes the author's own
  spelling (`pre_tool`, not `PreTool`) — a user comparing prompt to manifest
  should not have to wonder whether they match.

  **Signature validity and signer trust are kept apart.** `verify_archive`
  returns the *key identity*; nothing in that module returns a trust level.
  Verifying against a key that shipped inside the archive proves only internal
  consistency, which an attacker arranges trivially by signing their own
  payload — so `verify_from_trusted_key` is the form a registry client uses, and
  a test demonstrates the attack the loose form permits.

  **Skills port with zero edits, which drives the loader's tolerance.** Unknown
  frontmatter keys are kept, not rejected; a BOM is tolerated; a `---`
  horizontal rule in the body does not end the frontmatter early. An oversized
  body is *reported*, never silently truncated — spilling its tail belongs to
  whoever owns an artifact store.
- **M16.2** Skills index/lazy-body in harness assembly; two skills ported unmodified. ✅ *(shipped: `ContextBuilder::set_skills_index` / `load_skill_body`, the `load_skill` tool; skills in `crates/panday-harness/tests/skills/`.)*

  The index sits in the **stable** band and the bodies land in **semi-stable**,
  which is a cache-economics decision rather than a tidiness one: the index is
  paid for on every turn of the session, so it must cost a line per skill. A
  test asserts the sharper version of that — **the index's size does not change
  when the bodies grow 50×.** An arbitrary size ratio would only measure how
  verbose the fixtures happen to be.

  Loading a body **appends** and never rewrites the stable prefix, and loading
  the same skill twice is a no-op: docs/16 says a body "stays for the session
  (unloading churns cache)", and re-appending churns it just as badly.

  `load_skill` returns the body; the **actor** places it. A tool that could
  append to the stable region would be able to break the ADR-008 invariant from
  outside the component that guarantees it.

  A hallucinated skill name lists the ones that exist, so a wrong guess costs
  one recoverable turn rather than repeated guessing.

  *On "ported unmodified":* the two skills use the unmodified SKILL.md format,
  including frontmatter keys we do not model (`license`, `allowed-tools`,
  `version`, `author`), and the loader keeps them. They are representative of
  the ecosystem's format rather than copies of a third party's work — copying
  someone's skill verbatim into this repo is a licensing question, not a
  technical demonstration.
- **M16.3** MCP client host (stdio under T2): mount a public MCP server, call its tool through the loop with Ask-gating.
- **M16.4** WASM tool + hook runtime (wasmtime, WIT world v1); fuel/epoch limits enforced in escape suite. ✅ *(shipped: `panday_sandbox::t1_hook` + the `hook` world in `crates/panday-sandbox/wit/plugin.wit`, `panday_harness::{WasmPluginTool, WasmPluginHook}`; suites in `crates/panday-sandbox/tests/t1_hooks.rs` and `crates/panday-harness/tests/wasm_plugin.rs`; demo hook in `fixtures/demo-hook`.)*

  **"Vetoes limited to `pre_tool`" is a type, not a policy check.** In the WIT world
  only `pre-tool` returns a `verdict`; `post-tool` and `on-stop` return nothing, so
  there is no value for a caller to misread as a vote and no way for a plugin to try.

  **A budget breach is `Proceed`.** docs/16 says a hook that exceeds its 10ms budget
  "is skipped and the event logged", and docs/13 gives the same rule for any hook
  failure. Both other readings are wrong in a specific way: treating the breach as a
  veto lets a slow plugin disable a tool, and treating it as approval lets one bypass
  a policy by timing out. The breach is reported through `HookReporter`, because a
  skipped hook nobody hears about is the failure mode that reporter exists to prevent.

  **Redaction happens on our side of the boundary**, in the harness adapter rather
  than in the tier — it is the only place a plugin receives tool arguments, and doing
  it deeper would silently hold native hooks to a different standard. It is a denylist
  of key *shapes*, not a scan for secret-looking values: a value-based filter has to
  guess, and the token that does not match the guess is the one that leaks. Redacted
  values become `[redacted]` rather than disappearing, so a DLP hook can tell "there
  was a token here" from "there was no token". The end-to-end test is the plugin's own
  word for it — the demo hook vetoes if it ever sees `sk-live-`, and the control case
  (unredacted) proves the veto fires, so a redaction that stripped the whole object
  could not pass.

  **An unparseable rewrite is not a rewrite.** The hook's replacement arguments come
  back as a JSON string; if it does not parse, the original arguments proceed and the
  failure is reported. Substituting something the hook did not ask for would be worse
  than ignoring it.

  Plugin tools run on `spawn_blocking`: wasmtime's sync API blocks its thread and a
  guest may use its whole fuel budget, so running it on an async worker would stall
  every other session on that thread for as long as the budget allows. And a plugin
  tool reports `sandbox_tier: T1Wasm` with `SideEffects` from its manifest, which is
  what makes docs/16's claim exact — the loop cannot tell it from a native tool, the
  permission engine can, and sandbox-seconds land under the right tier (M21.2).
- **M16.5** ACP bridge: interactive session from Zed; permission round-trip works. *(The AEP⇄ACP mapping table landed early at M3.4 — `panday_cli::acp` — so what remains here is transport: stdio, the `session/new` handshake, and awaiting the client's permission answer.)*
- **M16.6** Registry service (publish/fetch/verify) + `panday plugin install`; marketplace UI deferred to phase 4.
