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

- **M22.1** dev compose (PG+MinIO+services) + one-command bootstrap (`just dev`).
- **M22.2** CI → staging deploy on merge; smoke suite (create session, run turn, check ledger row).
- **M22.3** Production shape 2 live with status page; backup/restore drill passes.
- **M22.4** T3 pool on KVM nodes with warm snapshots; chaos test: kill a pool node mid-exec, session resumes elsewhere.
- **M22.5** On-prem bundle v1 installed air-gapped following only its own README.
