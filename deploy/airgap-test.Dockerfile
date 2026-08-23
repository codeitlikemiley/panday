# The air gap, reproduced (docs/22 M22.5).
#
# Every property M22.5 asserts today is *negative* and checked at build time: the installer
# contains no network command, the README asks for nothing off-machine. None of that proves the
# install **succeeds** without a network, because the machine that runs the suite has one.
#
# `docker run --network none` is a real air gap for every property this kit claims — no interface
# but loopback, no DNS, no route. The two stages are the two machines M22.5 actually describes:
#
#   build      the release machine. Has a network, because a kit is *built* somewhere connected.
#   airgapped  the customer's box. Gets the tarball and nothing else.
#
# The image is the delivery; `--network none` at `docker run` is the locked door. Nothing in the
# second stage may install anything — the moment it runs `apt-get`, it is no longer the machine
# this test exists to imitate.

FROM rust:1-bookworm AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY xtask xtask
COPY proto proto
COPY fixtures fixtures
COPY docs docs

# One RUN, with the registry and `target/` on cache mounts. Split across layers they would be
# thrown away on every source change — a ten-minute rebuild to re-test a one-line README edit,
# which is how a test stops being run. The kit has to be written to `/kit`, outside the mounts:
# a cache mount is not part of the image, so anything left in `target/` would vanish here.
#
# The kit is assembled by the same `xtask airgap` a release would run, not hand-copied, or this
# would be testing a kit no customer ever receives.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked \
      -p panday-cli      --bin panday \
      -p panday-local    --bin panday-local \
      -p panday-gateway  --bin panday-gateway \
      -p panday-platform --bin panday-platform \
 && mkdir -p /models && head -c 4096 /dev/urandom > /models/tiny-test.gguf \
 && cargo run --release --locked -p xtask -- airgap --models /models --out /kit

# The stub is 4KB of noise, and it is honest about what it proves: that `models/` is packed,
# installed to `$MODEL_DIR`, and named in the README — a path no test had ever taken. It proves
# nothing about inference, which needs a runner the kit does not ship (docs/18 M18.7). Swapping in
# a real GGUF here would make the image gigabytes and still not answer a prompt.

FROM debian:bookworm-slim AS airgapped
# Nothing is installed here on purpose. A `RUN apt-get install` in this stage would be the test
# quietly handing the air-gapped machine something the kit did not ship.
COPY --from=build /kit /kit
WORKDIR /kit
