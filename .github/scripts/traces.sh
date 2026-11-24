#!/usr/bin/env bash
# Fetches netsim's Mahimahi traces (SPEC §23.1 tier 2) for ci.yml's netsim jobs.
# crates/netsim/traces.sum: sha256sum lines ("<sha256>  <file>", the file named by itself or by a
# path or URL ending in it) and the pinned ravinet/mahimahi commit as a 40-hex word. The traces go
# to crates/netsim/testdata/traces/ (git-ignored).
set -euo pipefail
cd "$(dirname "$0")/../.."
sums=crates/netsim/traces.sum dir=crates/netsim/testdata/traces
commit=$(grep -oE '\b[0-9a-f]{40}\b' "$sums" | head -1 || true)
[ -n "$commit" ] || { echo "$sums names no mahimahi commit"; exit 1; }
entries=$(grep -E '^[0-9a-f]{64} ' "$sums") || { echo "$sums lists no traces"; exit 1; }
mkdir -p "$dir"
while read -r sum file; do
  file=${file#\*}; file=${file##*/}
  curl -fsSL --retry 3 -o "$dir/$file" \
    "https://raw.githubusercontent.com/ravinet/mahimahi/$commit/traces/$file"
  echo "$sum  $dir/$file" | sha256sum -c -
done <<< "$entries"
