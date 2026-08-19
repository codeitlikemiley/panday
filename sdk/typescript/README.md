# @panday/sdk

**Generated. Do not edit — run `cargo xtask ts-sdk`.**

Types come from the same two sources the Rust implementation is checked against:

- `proto/aep-envelope.schema.json` — the event protocol (docs/03), generated from the Rust types.
- `panday_gateway::openapi` — the HTTP surface, which lives next to the handlers it describes.

CI regenerates this directory and fails if it differs, so a protocol change that forgets the SDK
breaks the build rather than a user's.

## Not published

`package.json` sets `"private": true`, and that is deliberate rather than pending. Publishing is a
commitment with a name on it — an npm scope, a release cadence, a deprecation policy, somebody who
answers issues — and a package on a registry without those is how an unmaintained SDK becomes
somebody's dependency. The generator is the part that had to exist first; the publish pipeline is a
decision for whoever takes that on.

Until then, vendor it: copy `src/` into your project, or add this directory as a path dependency.
Imports carry explicit `.ts` extensions because what ships here is source rather than a build
output — the specifier names the file that is actually on disk, which is what Deno, Bun and a
bundler-free Node all need.

## Two namespaces, on purpose

The HTTP surface is exported flat; the event protocol is under `aep`. They collide honestly:
`Usage` in the HTTP dialect counts prompt and completion tokens, while `Usage` in the event log
counts fresh input, cache reads and cache writes separately (ADR-007, because those price
differently). Renaming one to flatten them would produce a generated name that does not match the
protocol it came from.

```ts
import { type ChatCompletion, aep } from "./src/index.ts";

const envelope: aep.Envelope = JSON.parse(line);
```

## Use

```ts
import { Panday } from "./src/index.ts";

const panday = new Panday({ baseUrl: "http://127.0.0.1:8088", apiKey: process.env.PANDAY_API_KEY });

const completion = await panday.chat({
  model: "auto", // let the router choose (docs/12)
  messages: [{ role: "user", content: "why is this test failing?" }],
});

for await (const chunk of panday.stream({ model: "auto", messages: [...] })) {
  // chunks arrive parsed; the `[DONE]` sentinel is consumed, not yielded
}
```

The client does no retrying on purpose: the gateway already owns failover, circuit breaking and
rate limits (docs/11), and a client retrying on top of that turns one 429 into five.
