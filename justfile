# One-command workflows (docs/22 M22.1). `just` with no target lists them.
#
# The rule these follow: a target either does the whole thing or fails saying what is missing.
# Half-done setup that prints a hint is how a "one-command bootstrap" becomes five commands.

default:
    @just --list

# ── dev shape 1 ────────────────────────────────────────────────────────────────
compose := "docker compose -f deploy/compose/dev.yml"
dev_db := "postgres://panday:panday@127.0.0.1:5442/panday"

# Bring up the dev stack, migrate it, and mint an API key you can actually call with.
dev: dev-up
    @echo "→ migrating"
    @PANDAY_DATABASE_URL="{{dev_db}}" cargo run --quiet -p panday-platform -- migrate
    @echo "→ minting a dev account and key"
    @PANDAY_DATABASE_URL="{{dev_db}}" scripts/dev-key.sh

# Infra only: Postgres and MinIO, waited on until healthy.
dev-up:
    {{compose}} up -d --wait

# Build and run the platform in a container too — the deployable artifact, not the dev loop.
dev-services:
    {{compose}} --profile services up -d --wait --build

dev-down:
    {{compose}} down

# Stop and delete the data. The one destructive target, named so nobody types it by accident.
dev-reset:
    {{compose}} down -v

dev-logs:
    {{compose}} logs -f

# Run the platform on the host against the compose database — the normal dev loop.
serve:
    PANDAY_DATABASE_URL="{{dev_db}}" PANDAY_PLATFORM_ADDR=127.0.0.1:8088 \
      cargo run -p panday-platform -- serve

# ── the gate ───────────────────────────────────────────────────────────────────
# What CI runs, in the order that fails fastest.
check: fmt-check lint test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo nextest run --workspace

# Postgres-backed tests, against the integration compose rather than the dev one.
it:
    # A separate database on purpose: `just it` must never touch the one holding your dev key.
    docker compose -f deploy/integration-compose.yml up -d --wait
    PANDAY_TEST_DATABASE_URL=postgres://panday:panday@127.0.0.1:5433/panday_test \
      cargo nextest run --run-ignored all -E 'package(panday-platform)'

deny:
    cargo deny check

# The evals that need a model. Run with `llama-server` up, or against `just serve`.
# `just check` deliberately does not include this: CI has no model, and a suite that skips is worse
# than one that is absent.
bench model="local/qwen3.5-4b" url="http://127.0.0.1:8088":
    cargo run -p xtask -- json-bench --model {{model}} --base-url {{url}} --write

# Regenerate everything that is checked in and derived: schemas, SBOM, the TypeScript SDK.
generated:
    cargo xtask schemas
    cargo xtask sbom
    cargo xtask ts-sdk

schemas:
    cargo xtask schemas
