#!/usr/bin/env bash
# docs/02 M2.4: "binaries under 25MB".
#
# A size budget only works if something enforces it. Binary size creeps one
# dependency at a time and nobody notices until a release is 80MB, so this
# fails the build rather than printing a warning.
set -euo pipefail

DIR="${1:?usage: check-binary-sizes.sh <release-dir>}"
LIMIT_BYTES=$((25 * 1024 * 1024))

# The binaries we actually ship. Others in the directory are build artifacts.
BINARIES=(panday panday-gateway panday-harnessd panday-local panday-platform)

fail=0
found=0
for name in "${BINARIES[@]}"; do
  path="$DIR/$name"
  [ -f "$path" ] || continue
  found=$((found + 1))

  # BSD stat (macOS) and GNU stat (Linux) disagree on flags.
  size=$(stat -f%z "$path" 2>/dev/null || stat -c%s "$path")
  mb=$(awk "BEGIN {printf \"%.1f\", $size/1048576}")

  if [ "$size" -gt "$LIMIT_BYTES" ]; then
    printf '  %-20s %6s MB  OVER the 25MB budget\n' "$name" "$mb"
    fail=1
  else
    printf '  %-20s %6s MB  ok\n' "$name" "$mb"
  fi
done

if [ "$found" -eq 0 ]; then
  echo "no shipped binaries found in $DIR — a passing size check on zero binaries is not a pass"
  exit 1
fi

if [ "$fail" -ne 0 ]; then
  echo "binary size budget exceeded (docs/02 M2.4)"
  exit 1
fi
echo "all $found binaries within the 25MB budget"
