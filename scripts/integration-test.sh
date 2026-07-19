#!/usr/bin/env bash
# Run the real two-process cold-start integration test: it builds both binaries
# (the Go `ccs` control plane and the Rust `ccs-proxy` data plane) and drives
# them as separate processes, asserting cold-start URL minting, warm reuse, and
# proxy-restart port stability. Build-tagged `integration`, so it is excluded
# from the default `go test ./...` CI gate; this script is the convenience
# wrapper for the build-then-test sequence.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "==> cargo build -p ccs-proxy"
cargo build -p ccs-proxy --manifest-path "$repo_root/crates/Cargo.toml"

echo "==> go test -tags integration ./internal/integration/"
cd "$repo_root/go"
go test -count=1 -tags integration ./internal/integration/ -v "$@"
