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

**Except `net`, which is requested and never granted.** No tier can enforce a
per-domain allowlist — the egress proxy `docs/14` §policy describes is not
built — and since M14.8 a `NetPolicy` naming a host is refused outright rather
than approximated. A plugin declaring `net` therefore gets *no* egress, and the
consent prompt says so in those words rather than listing the hosts as though
they were granted. Sentence corrected here because it claimed enforcement that
does not exist, which is the specific failure `panday_plugins`' own note warns
about: "a capability that grants nothing in the sandbox is a lie told at the
consent prompt." The field stays in the manifest so the requirement can be
expressed once and mean something the day the proxy lands.
Distribution: `.plugin` archive, ed25519-signed; registry tiers
`verified | community | unlisted`, with the marketplace being a phase-6
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
- **M16.3** MCP client host (stdio under T2): mount a public MCP server, call its tool through the loop with Ask-gating. ✅ *(shipped: `panday_plugins::mcp`, `panday_harness::McpTool`; suites in `crates/panday-plugins/tests/mcp_host.rs` and `crates/panday-harness/tests/mcp_in_the_loop.rs`.)*

  **stdio only, by compilation.** `rmcp` is built with `transport-child-process` and
  nothing that opens a socket, so this host cannot reach the network even by mistake.
  The streamable-HTTP transport and OAuth are a separate decision with a separate
  threat model — a remote MCP server is a third party reading your prompts — and they
  are deliberately unwired.

  **`Irreversible` is the default, and MCP's own hints do not change it.** The
  protocol carries `readOnlyHint`/`destructiveHint`, but those are the *server's*
  claims about itself, and treating an unverified claim as a permission grant is how
  a consent model becomes decorative. So every mounted tool asks in every profile
  until a human grants it, per tool — consenting to `github:list_issues` is not
  consenting to `github:delete_repo` — and a grant relaxes the gate without touching
  the tier: the server is still a child process, and its seconds still meter as T2.
  Nothing is replay-safe: re-running a `create_issue` after a crash reaches somebody's
  inbox.

  **The server inherits no environment** (`env_clear`, then only what the manifest
  declared), which is T2's `--clearenv` rule applied to third-party code. Both halves
  are tested: a canary variable is invisible, and a declared one does arrive —
  without the second test the first would pass with a broken `env()` builder.

  **The test server is hand-written**, not built with `rmcp`'s server half: "mount a
  public MCP server" means talking to somebody else's implementation, and a test where
  both ends come from one crate proves only that the crate agrees with itself. The
  fixture speaks line-delimited JSON-RPC the way a Python or TypeScript server does,
  including returning no response to a notification — the detail a naive server gets
  wrong and then hangs a client on.

  Schema hygiene is `MountedTool::brief()`: a tool schema lands in the *stable* cached
  prefix (ADR-008), so a 40kB schema is 40kB paid on every turn of the session. Tools
  are captured at mount rather than re-listed per call, because a server changing its
  tools mid-session is a cache break, and docs/13 makes that a deliberate logged act
  rather than something a third party can do to us silently.

  **One dependency conflict, resolved on the record.** `rmcp` depends on `chrono`
  unconditionally on non-wasm targets, and docs/02 bans chrono ("pick ONE — we pick
  `time`"). The ban's purpose is that *our* code has one date library, and that still
  holds: `deny.toml` now allows chrono only with `wrappers = ["rmcp"]`, so anything
  else pulling it in still fails the build.
- **M16.4** WASM tool + hook runtime (wasmtime, WIT world v1); fuel/epoch limits enforced in escape suite. ✅ *(shipped: `panday_sandbox::t1_hook` + the `hook` world in `crates/panday-sandbox/wit/plugin.wit`, `panday_harness::{WasmPluginTool, WasmPluginHook}`; suites in `crates/panday-sandbox/tests/t1_hooks.rs` and `crates/panday-harness/tests/wasm_plugin.rs`; demo hook in `fixtures/demo-hook`.)*

  **"Vetoes limited to `pre_tool`" is a type, not a policy check.** In the WIT world
  only `pre-tool` returns a `verdict`; `post-tool` and `on-stop` return nothing, so
  there is no value for a caller to misread as a vote and no way for a plugin to try.

  **The budget covers the plugin's code, not our linking.** CI caught this: docs/16 gives
  a hook 10ms, and instantiating a component on a loaded runner takes longer than that, so
  every hook was reported over budget before its own code ran — a hook that does nothing
  but log failed with `Deadline(10ms)`. Fuel and the epoch deadline are now armed *after*
  instantiation, with a generous bootstrap allowance for the linking itself. A budget the
  runtime can exhaust on its own is not a budget on the guest.

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
- **M16.5** ACP bridge: interactive session from Zed; permission round-trip works. ✅ *(shipped: `panday_cli::acp_server` + `panday acp`; suite in `crates/panday-cli/tests/acp_bridge.rs`. The mapping table landed at M3.4.)*

  **What is verified, and what is not.** Zed cannot run in CI, so the suite drives our
  agent with the official crate's own `Client` role over an in-memory `Channel`: a real
  ACP conversation — initialize, `session/new`, `session/prompt`, `session/update`
  notifications, `session/request_permission` — in the order an editor does it, over the
  protocol it speaks. "It works in Zed" remains a claim someone verifies by hand once.

  **The deadlock the crate warns about, made twice.** A turn asks the *client* for
  permission mid-way. Awaiting that from inside the `session/prompt` handler blocks the
  dispatch loop, so the loop cannot deliver the answer the handler is waiting for — the
  crate documents this on `block_task` with a "❌ DEADLOCK" example, and the first draft
  did it anyway: the whole suite hung with no output. The turn now runs in a spawned task
  and responds from there, which works because `Responder` is `Send` and `respond`
  consumes it.

  **A found bug in the harness.** Making the bridge compile required the turn future to
  be `Send`, and it was not: `execute()` held a `tracing` span *guard* across the tool
  await, and `record_outcome()` held one across the commit. A guard is thread-local
  (docs/21 M21.1's lesson) **and** `!Send`, so those two sites were silently losing their
  spans on the multi-thread runtime as well as making the turn unusable from a spawned
  task. Both are `.instrument()`/`in_scope()` now. The ACP work found a telemetry bug,
  which is the sort of thing that only turns up when a second caller appears.

  `panday acp` defaults to the `dev` profile rather than `unleashed`: an editor session
  has a human in it, and the point of the gate is that they see the question. The jail's
  environment is five toolchain variables, never the parent environment (docs/20 T4).
- **M16.6** Registry service (publish/fetch/verify) + `panday plugin install`; marketplace UI deferred to phase 6. ✅ *(shipped: `panday_plugins::archive`, `panday_platform::registry` (+ its `http` router), `panday plugin install`; suites in `crates/panday-plugins/tests/archive.rs`, `crates/panday-platform/tests/registry.rs`, `crates/panday-cli/tests/plugin_install.rs`.)*

  **The order is the security property**: fetch → verify → consent → extract.

  - Verify *before* reading the manifest, because the manifest is what the consent prompt
    quotes: an unverified one means consenting to text an attacker chose.
  - Consent *before* extracting. docs/16 says "install-time consent"; a prompt shown after the
    files are on disk is a notification. Without `--yes` the command prints every grant and
    writes nothing.
  - Extract into a fresh directory, refusing to overwrite — an install that could replace a
    file is an install that can be *used* to replace a file.

  **Extraction is written out rather than delegated.** `tar`'s `unpack` is one line and would
  create a symlink entry pointing at `~/.ssh`. So each rule is explicit and tested: no absolute
  paths, no `..` (refused lexically, not resolved — "resolves inside" depends on what the
  archive created first, and it controls that order), regular files and directories only, caps
  on entry count, entry size and total size, and the size checked against the *stream* rather
  than the header, since a header can claim one size and deliver another. The hostile fixtures
  are built by hand because `tar`'s builder refuses to write them — a real attacker is not using
  our packer.

  **Three registry properties, each a test.** A publisher cannot self-assign a tier: `publish`
  takes no tier argument at all, so there is no code path from a wish to a `verified`, and
  promotion is a store method with *no HTTP route* because that route needs authentication
  (M17.3) and an unauthenticated one would undo the trust model with a curl. A published version
  is immutable: re-publishing `name@version` is refused, since "same version, different code" is
  the supply-chain attack signing exists to prevent. And the archive's manifest must agree with
  the name it is published under, or every later consent prompt describes a different plugin
  than the one installed.

  **Trust on first use, said out loud.** `--trust <key>` means "this publisher or nobody".
  Without it the key is accepted *and printed*, with a line saying it was taken on trust and how
  to pin it next time — the first install cannot be verified against anything, and silence would
  let a user believe it was. The key is written next to the plugin so the next version can be
  pinned and a reviewer can see who signed what is on disk. A `verified` tier changes what the
  prompt says and nothing about what is enforced: the sandbox tiers and the manifest are what
  constrain a plugin.

  `tar` + `flate2` added (pure Rust, `rust_backend`, no C and nothing to cross-compile).
