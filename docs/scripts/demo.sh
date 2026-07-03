#!/usr/bin/env bash
# Regenerates docs/assets/demo.png from a real `ccs --help` run.
#
# Builds ccs from the working tree, captures its help output, and renders it
# with freeze (https://github.com/charmbracelet/freeze) in the house style.
# Run from anywhere: paths resolve relative to this script.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

go -C "$repo/go" build -o "$tmp/ccs" ./cmd/ccs

{
  printf '$ ccs --help\n'
  "$tmp/ccs" --help
} > "$tmp/demo.txt"

freeze "$tmp/demo.txt" \
  --language text \
  --theme github-dark \
  --background "#0d1117" \
  --window \
  --padding 24 \
  --font.size 28 \
  --output "$repo/docs/assets/demo.png"

# One lossy quantize pass to keep the asset under 1 MiB.
pngquant --force --skip-if-larger --output "$repo/docs/assets/demo.png" \
  -- "$repo/docs/assets/demo.png" || true
