# The deployable `panday-platform` (docs/22 shape 2).
#
# Multi-stage: the toolchain is ~1.5GB and none of it belongs in the thing that runs. The runtime
# image is debian-slim rather than distroless because the platform links OpenSSL through sqlx's TLS
# and needs the CA bundle to reach a managed Postgres — a static image that cannot verify a
# certificate is a smaller image that does not work.
FROM rust:1-bookworm AS build
WORKDIR /src

# The manifests first, so a source-only change does not re-resolve and re-download the whole
# dependency graph. `cargo fetch` is the layer worth caching; the build below is not.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY xtask xtask
COPY proto proto
COPY fixtures fixtures
COPY docs docs
RUN cargo fetch --locked

RUN cargo build --release --locked -p panday-platform --bin panday-platform

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged: the platform reads a database and serves HTTP. Nothing it does needs root, and a
# container that runs as root is one container escape away from being a host compromise.
RUN useradd --system --uid 10001 --create-home panday
USER panday

COPY --from=build /src/target/release/panday-platform /usr/local/bin/panday-platform

EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/panday-platform"]
CMD ["serve"]
