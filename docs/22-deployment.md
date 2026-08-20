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
- **M22.4** T3 pool on KVM nodes with warm snapshots; chaos test: kill a pool node mid-exec, session resumes elsewhere.
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

  **What remains** is the air: this machine has a network, so nothing here proves the install
  *succeeds* without one. What is proven is that it never asks for one.
