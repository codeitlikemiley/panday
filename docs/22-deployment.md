# 22 — Deployment

Three shapes (01), one philosophy: **boring, few moving parts, upgrade paths
written down before they're needed** (ADR-003).

## Shape 1 — dev/solo (day 1)

`panday local` for offline work; for cloud-ish dev, `docker compose up`:
Postgres 16 (pgvector image), MinIO, and the services as one compose file in
`deploy/compose/dev.yml`. Everything runs on a laptop.

## Shape 2 — SaaS (phase 3)

- **Compute**: containers on Fly.io / Railway / a plain VM with systemd to
  start — *not* Kubernetes. K8s enters only with the T3 sandbox pool if the
  provider can't give KVM another way (Fly Machines are Firecracker-based —
  evaluate running T3 *as* Fly machines before building a pool manager).
- **Services**: `panday-platform` (API), `panday-harnessd`, `panday-gateway`
  — each stateless, horizontally scalable; session-actor affinity via
  consistent hashing on session_id at the LB (an actor lives on one node;
  failover = fold the log on another).
- **Postgres**: managed (Neon/RDS/Supabase-postgres) with PITR. One primary;
  read replicas only when measured.
- **Object storage**: S3/R2 (R2's zero egress is attractive for artifact-heavy
  workloads).
- **Sandbox pool** (T3): dedicated Linux/KVM nodes (bare-metal-ish: Hetzner
  AX / OVH / EC2 metal), warm snapshot pools, autoscaled by queue depth.
  This is the one genuinely stateful-ish fleet; it gets its own runbook.
- **Secrets**: provider secret manager or sops+age in the repo for the small
  stage; never in env-committed files.

## Shape 3 — enterprise/on-prem

The SaaS compose, packaged: container images + a compose/helm bundle + signed
model catalog + entitlement token (17/18). No egress required; updates ship
as versioned bundles. Support boundary documented per bundle version.

**Shape 3 is not built.** What exists is the *shape 1* air-gap kit — one directory of binaries,
config and models (M18.7). The compose/helm bundle, the container images and the signed catalog
that make this shape 3 are unwritten, and widening M22.5's v1 to include them is a scope decision
nobody has taken. The boundary below is therefore the boundary of the kit that ships today.

### Support boundary, per bundle version

What a customer with `panday-airgap-<version>` may expect, and what is out of scope. The point of
writing it down is that an air-gapped customer cannot ask: they have the tarball, `INSTALL.md`, and
no channel to us until someone carries a question back through the door.

| | In the bundle | Not in the bundle |
|---|---|---|
| **Binaries** | `panday`, `panday-local`, `panday-gateway`, `panday-platform`, built for one target triple per bundle | Any other architecture. A bundle is not portable across them; ordering the wrong one is a return trip. |
| **Inference runner** | Only if the bundle was built with `xtask airgap --runner <path>`, and `INSTALL.md` says which | Otherwise nothing. `panday-local --serve` spawns `llama-server` by name, so a bundle without one installs and cannot answer a prompt. The customer carries their own through the door. |
| **Models** | The GGUFs packed at build time, listed by name in `INSTALL.md` | Any later model. There is no download path; a new model is a new bundle or a file copied in by hand. |
| **Entitlement** | Verification, locally, against `PANDAY_ENTITLEMENT_KEY` | Activation, revocation, or any call home. An expired token degrades to the community tier rather than stopping — that is the designed behaviour, not a grace period (M17.6). |
| **Updates** | A newer bundle, installed by re-running `install.sh` over the old one | In-place patching, delta updates, or anything that reaches a repository. Event logs under `.panday/` are append-only and survive an install (ADR-002). |
| **Data** | The customer's, on their disk, in SQLite and JSONL they can read | Telemetry. Nothing leaves the machine, so nothing can be sent to us for diagnosis — a bug report is whatever the operator can copy out by hand. |
| **Verification** | `install.sh` and `INSTALL.md` are tested against a machine with no network on every change (`xtask/tests/airgap_container.rs`) | A guarantee about the customer's specific host. What is tested is Debian-family x86-64 in a container; a different distribution, an SELinux policy or a read-only `$HOME` is untested ground. |

**What a version number covers.** A bundle is the binaries, the config and the models it was built
with, together. Mixing them across versions is unsupported: the routing policy and the catalog are
read by the binaries beside them, and a `catalog.yaml` from a newer bundle can name a model the
older binary cannot load. `install.sh` overwrites all three for that reason.

## Upgrade paths (pre-written triggers, per ADR-003)

| Pressure | Trigger metric | Move |
|---|---|---|
| queue throughput | PG queue >1k msg/s sustained or lock contention visible | NATS JetStream |
| analytics load | OLTP p99 degraded by dashboard queries | ClickHouse for events/metrics projections |
| vector scale | index > RAM, recall/latency degrading | VectorChord / pgvectorscale |
| cache latency | exact-cache PG p99 > 5ms | Redis |
| orchestration | sandbox fleet ops > 1 human-day/week | k8s for the pool only |

## Release engineering

- Tagged releases build: linux x86_64 + aarch64 (musl where possible), macOS
  arm64; signed; SBOM attached (20).
- DB migrations: sqlx migrate, forward-only, deploy = migrate-then-roll.
- Feature flags: a config table, not a vendor; kill switches for reducer,
  semantic cache, each provider, each plugin.
- Canary: harnessd rolls to 5% of sessions first (new sessions only — actors
  make this trivial); gateway rolls behind a header-routed canary lane.

## Milestones

- **M22.1** dev compose (PG+MinIO+services) + one-command bootstrap (`just dev`). ✅ *(shipped: `deploy/compose/dev.yml`, `deploy/Dockerfile`, `justfile`, `scripts/dev-key.sh`, and the `panday-platform` service binary that is the thing being brought up.)*

  **`just dev` mints a key, because a stack you cannot call is not up.** It brings the containers up
  waited-on-healthy, migrates, creates an account and prints an API key with the `curl` that uses
  it. The alternative — infra up, then read three docs to discover you need a key and a subcommand
  that mints one — is the "one-command bootstrap" that takes five commands.

  **The platform binary is the composition root, and this is where that becomes visible.** Every
  other binary is a deployment shape with a piece deliberately missing: `panday-gateway` has no
  database (it *cannot* — `panday-platform` depends on it, not the reverse), `panday local` has no
  accounts (ADR-011). Only this one wires the ledger to the gateway's usage sink, the key table to
  the ingress, and the route audit to the router. It also carries the bootstrap subcommands
  (`migrate`, `account`, `issue-key`, `keys`, `revoke-key`, `prune-routes`): the first key on a
  fresh deployment has to come from somewhere, and "somewhere" being a second tool nobody built is
  how a service ships without a way to use it. M17.7's admin panel replaces the ergonomics, not the
  need.

  **Migrations are compiled into the binary.** A container has no source tree, and
  `CARGO_MANIFEST_DIR` is a path on a build machine. `pg::EMBEDDED_MIGRATIONS` is listed by hand
  with a test asserting it matches `migrations/` — so adding a file and forgetting the list fails in
  CI rather than at the first boot after a deploy.

  **Three databases, three ports, on purpose.** 5432 is whatever the developer already runs, 5433 is
  the integration lane (tmpfs, fsync off, disposable by definition), 5442 is this one — named
  volumes and fsync on, because it holds the account and key you just minted. `just it` and `just
  dev` can run at the same time without one migrating over the other mid-test, and no target of
  either can wipe a developer's own database.

  **The services are behind a compose profile.** `docker compose --profile services up` builds the
  deployable image; the default is infra only, because during development the service belongs on the
  host where a rebuild is seconds rather than a container image. The image itself is multi-stage,
  runs as an unprivileged user, and is debian-slim rather than distroless — the platform needs a CA
  bundle to reach a managed Postgres, and a smaller image that cannot verify a certificate is a
  smaller image that does not work.

  **MinIO is in the file with nothing reading it yet.** The artifact store (docs/15 §spilling) lands
  later; having it here means the dev shape does not change when it does. Stated rather than
  discovered, because a compose service nobody consumes is a service nobody notices is broken.
- **M22.2** CI → staging deploy on merge; smoke suite (create session, run turn, check ledger row). ✅ *(shipped: `crates/panday-platform/tests/smoke.rs`, `.github/workflows/deploy.yml`. **No staging host is configured**, so the workflow announces that and does nothing.)*

  **The smoke suite is the part that had to be real, and it is.** Four assertions against a running
  deployment: a freshly minted key runs a turn through the ingress; the same request without a key
  is refused; the ledger has a row when the turn actually reached a provider; and the schema is the
  one this binary carries. That third one is the point of the whole exercise — a deploy where
  inference works and metering silently does not is the worst possible green tick, because it looks
  fine until the invoice.

  **A 503 is a pass.** A deployment with no provider configured still proves auth, routing and the
  error envelope; demanding a 200 would mean the smoke suite could only run where somebody was
  paying for tokens. What it will not do is assert a ledger row for a call that never happened.

  **The schema check exists because of a specific failure**: a binary rolled without its migrations
  passes every other assertion here and then fails on the first query against a column that does not
  exist.

  **The workflow does nothing until it is configured**, guarded on `STAGING_DEPLOY_HOST` — a fork or
  a clone should not fail a build over a deployment that does not exist. What it does *not* do is
  skip the smoke: if a deploy happens the suite runs, and if the suite fails the deploy is reported
  failed. The ship step is deliberately one unabstracted command, because docs/22 chose "containers
  on Fly.io / Railway / a plain VM with systemd", and an abstraction over "how do I ship a
  container" is a thing that breaks on the day you need to read it.

  **Verified against a real deployment**: the suite was run against the dev stack (`just dev` plus
  `panday-platform serve`) and passes there, which is the same code path a staging host would take.
  What remains unclosed is the *host* — nothing has been provisioned to deploy to.
- **M22.3** Production shape 2 live with status page; backup/restore drill passes. **Partial** *(shipped: `GET /status`, and the drill — `scripts/backup-drill.sh` / `just drill`, run and recorded in docs/20 M20.4. **"Live" needs a host.**)*

  **The status endpoint is unauthenticated and content-free.** An uptime checker cannot present a
  key, so a status page behind auth is one nobody reads; and account counts on a public URL are a
  business metric anyone can scrape. It reports three things: the build, whether the database
  answers, and whether the schema matches this binary.

  **It returns 503 when degraded, not 200 with a sad word in the body** — every uptime checker in
  the world reports the second as up.

  **What is missing is the deployment itself.** The image builds, the migrations run on boot behind
  an advisory lock, the drill restores to identical numbers, and the smoke suite passes against a
  running instance. Nothing has been pointed at a production host, and pretending otherwise would be
  the one claim in this repo that a reader could not check.
- **M22.4** T3 pool on KVM nodes with warm snapshots; chaos test: kill a pool node mid-exec, session resumes elsewhere. **Partial** *(shipped: `panday_sandbox::t3::nodes` — the consistent-hash ring, `drain`, and the failover suite in `crates/panday-harness/tests/node_failover.rs`. **The KVM nodes are missing**, so the timing half of the chaos test is not measured.)*

  **Failover needs no migration, and the test says so by doing none.** A second node rebuilds the
  session by calling `resume()` over the same log — nothing is copied, no handoff is coordinated, no
  state is drained, because the log *is* the state (ADR-002). That is why killing a node mid-session
  is a latency event rather than a data-loss event.

  **`resume()` is not optional, and the suite proves it.** A fresh actor that skips it starts
  numbering at seq 1 and collides with the log it inherited — `SeqConflict(1)`, which is exactly what
  a naive failover would hit in production the first time it happened.

  **A mid-flight session folds to mid-flight.** Kill a node while a permission decision is
  outstanding and the fold reports `Gating` with the call still parked and *not* counted as
  dispatched. The failure this guards against is a resumed node inventing an outcome for a call
  nobody answered.

  **Consistent hashing, for the reason it exists.** Adding a fourth node to three moves roughly a
  quarter of sessions rather than all of them, and a node's death moves only *its* sessions — a
  drain that reshuffled healthy ones would turn one node's failure into every session's cold start.
  Both are asserted, and so is the invariant a load balancer depends on: while any node is alive, no
  session is homeless.

  **A distribution test caught a real bug in the hash.** Plain FNV-1a over sequential session ids
  varies almost entirely in the low bits, so 1,000 sessions landed 200/100/500/200 across four nodes
  and adding a fifth moved *zero* keys — a ring with none of the properties a ring is for. The fmix64
  finalizer spreads those differences across all 64 bits. Worth recording because the test that
  caught it exists only to catch it: every other assertion passed while the ring was useless.

  **What needs real nodes** is the timing: how long a fold takes on a cold cache, and whether the
  load balancer notices a death before the client does.
- **M22.5** On-prem bundle v1 installed air-gapped following only its own README. **Partial** *(shipped: `xtask::airgap` with the kit's offline claim enforced at build time and asserted in the suite. **The air-gapped machine is still missing** — nothing has installed this behind a locked door.)*

  M22.5's bar is "installs following only its own README". Two halves, and only one of them needs
  hardware.

  **The contents are checkable, so they are checked.** The installer must reach no network — every
  `curl`, `wget`, `brew`, `apt`, `pip`, `npm`, `scp`, `ssh` and friend is forbidden, verified in the
  suite *and* by the builder itself, which refuses to write a kit whose installer contains one. The
  cheapest moment to catch a `curl` creeping in is before the tarball reaches somebody with no
  network to use it on.

  **The README is checked against the box.** Every component in `KIT_LAYOUT` must appear in the
  README, and the README must ask for nothing from the network — because it is the only
  documentation the reader has, and a path in it that the kit does not ship is a dead end behind a
  locked door. The layout list is the same one the builder creates directories from, so a component
  added to the kit cannot be missing from its own documentation.

  **Documentation drift is a test failure.** The README names `PANDAY_ENTITLEMENT_KEY` and
  `--entitlement`, and the suite asserts those are the names the binary actually reads (M17.6) —
  a customer who cannot activate their licence behind an air gap also cannot ask.

  **The installer's own properties**: `set -eu` so a half-install cannot report success on a machine
  nobody can ssh into; `cp` rather than `ln -s`, because a USB stick that gets unplugged is not a
  storage backend; and every destination under `$PREFIX`, `$MODEL_DIR` or `$HOME`, so it never needs
  `sudo` it did not warn about.

  **The air is reproducible, and reproducing it found a real defect.**
  `docker run --network none` is a real air gap for every property this kit claims — loopback and
  nothing else, no DNS, no route. `deploy/airgap-test.Dockerfile` builds the kit on a connected
  stage and copies it into a bare `debian:bookworm-slim` that installs nothing: the two machines
  this milestone actually describes. `xtask/tests/airgap_container.rs` then installs there and
  runs what the README tells a reader to run.

  The first thing it caught: **the README's only verification step could not execute.**
  `panday-local --serve` spawns `llama-server` (`panday_local::supervisor`), the kit shipped no
  runner, and an air-gapped machine cannot fetch one. The box claimed "everything needed to run
  Panday" and could not answer a prompt. Every check that existed here was a string property of
  two generated files — one of them asserted the README *contains* `panday-local --serve` — so the
  text was verified and the runnability never was. Fixed under M18.7 with `xtask airgap --runner`
  and a README that adapts; the container test is what keeps it fixed.

  The suite asserts the air gap itself before anything else (`the_air_gap_is_real`): a container
  that could still resolve a hostname would install happily whether or not the kit needed a
  network, and every other assertion here would be worthless.

  **What remains** is a machine with no network *and no Docker* — one real box, installed from
  `INSTALL.md` by someone who has not read this repo. The container proves the installer needs no
  network; it does not prove the README is followable by a stranger.
