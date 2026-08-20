#!/usr/bin/env bash
# Build a golden rootfs for T3 (docs/14 §T3, M14.5).
#
# > "Rootfs = overlay over a golden image per toolchain"
#
# One image per toolchain, built from a Dockerfile so the contents are a reviewable text file rather
# than a tarball somebody made once on a laptop. The output is an ext4 image Firecracker can boot,
# plus a sha256 next to it — an unverified rootfs is an unverified supply chain, and this one runs
# strangers' code.
#
#   scripts/build-rootfs.sh rust            # → target/rootfs/rust.ext4
#   SIZE_MB=4096 scripts/build-rootfs.sh python
#
# Linux only, and needs docker. It is not run in CI: building a bootable image is minutes of I/O and
# nothing in CI can boot the result. The person with the KVM host runs it, and the hash is what
# everyone else checks.
# dangerous-strings: data-only — `mkfs.ext4` below targets a regular file this script just created
# with `dd`, never a block device; the lint's pattern cannot tell the two apart.
set -euo pipefail

TOOLCHAIN="${1:-rust}"
SIZE_MB="${SIZE_MB:-2048}"
OUT_DIR="${OUT_DIR:-target/rootfs}"
OUT="$OUT_DIR/$TOOLCHAIN.ext4"

case "$(uname -s)" in
  Linux) ;;
  *)
    echo "build-rootfs.sh needs Linux: it makes an ext4 filesystem and mounts it." >&2
    echo "docs/14: macOS users get T2 locally; cloud execution is always Linux." >&2
    exit 1
    ;;
esac

command -v docker >/dev/null || { echo "docker is required" >&2; exit 1; }
command -v mkfs.ext4 >/dev/null || { echo "e2fsprogs is required" >&2; exit 1; }

case "$TOOLCHAIN" in
  rust)   BASE="rust:1-slim-bookworm" ;;
  python) BASE="python:3.12-slim-bookworm" ;;
  node)   BASE="node:22-bookworm-slim" ;;
  base)   BASE="debian:bookworm-slim" ;;
  *) echo "unknown toolchain `$TOOLCHAIN` (rust|python|node|base)" >&2; exit 1 ;;
esac

mkdir -p "$OUT_DIR"
WORK="$(mktemp -d)"
# `${WORK:?}` rather than `$WORK`: an empty variable must abort, not expand to nothing and leave
# `rm -rf` pointed at the root.
trap 'sudo umount "${WORK:?}/mnt" 2>/dev/null || true; rm -rf "${WORK:?}"' EXIT

echo "== 1. assemble the filesystem from $BASE"
# `init` is the guest agent: pid 1 in a microVM with no init system. It is built statically so the
# rootfs needs no matching libc, and it is the only thing standing between a booted VM and a useless
# one — Firecracker boots a kernel, not a container.
cat > "$WORK/Dockerfile" <<DOCKER
FROM $BASE
RUN apt-get update \\
 && apt-get install -y --no-install-recommends ca-certificates iproute2 \\
 && rm -rf /var/lib/apt/lists/*
# The agent's own dependencies are none: it speaks a line protocol over vsock and runs commands.
COPY panday-guest /usr/bin/panday-guest
RUN mkdir -p /workspace \\
 && ln -sf /usr/bin/panday-guest /sbin/init
DOCKER

if [ ! -f "$WORK/panday-guest" ]; then
  # Not built here on purpose: the guest agent is a Rust binary from this workspace, and building it
  # inside this script would hide a cross-compilation step that deserves to be explicit.
  echo "   no guest agent supplied — set GUEST_AGENT=<path> once panday-guest exists (M14.6)."
  echo "   building a rootfs without one produces an image that boots to nothing."
  : > "$WORK/panday-guest"
  chmod +x "$WORK/panday-guest"
fi
[ -n "${GUEST_AGENT:-}" ] && cp "$GUEST_AGENT" "$WORK/panday-guest"

docker build --quiet --platform linux/amd64 -t "panday-rootfs-$TOOLCHAIN" "$WORK" >/dev/null
CONTAINER="$(docker create "panday-rootfs-$TOOLCHAIN")"

echo "== 2. make the image"
dd if=/dev/zero of="$OUT" bs=1M count="$SIZE_MB" status=none
mkfs.ext4 -q -F "$OUT"
mkdir -p "$WORK/mnt"
sudo mount -o loop "$OUT" "$WORK/mnt"
docker export "$CONTAINER" | sudo tar -x -C "$WORK/mnt"
docker rm "$CONTAINER" >/dev/null
sudo umount "$WORK/mnt"

echo "== 3. hash it"
# The hash is the artifact anyone else checks: a golden image is only golden if two people can agree
# they have the same one.
sha256sum "$OUT" | tee "$OUT.sha256"

printf '\nbuilt %s (%s MB)\n' "$OUT" "$SIZE_MB"
echo "Boot it with: panday-sandbox's T3 tier, kernel from KERNEL_PATH, this as the root drive."
