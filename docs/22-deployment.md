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
- **M22.2** CI → staging deploy on merge; smoke suite (create session, run turn, check ledger row).
- **M22.3** Production shape 2 live with status page; backup/restore drill passes.
- **M22.4** T3 pool on KVM nodes with warm snapshots; chaos test: kill a pool node mid-exec, session resumes elsewhere.
- **M22.5** On-prem bundle v1 installed air-gapped following only its own README.
