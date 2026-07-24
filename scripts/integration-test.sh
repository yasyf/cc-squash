#!/usr/bin/env bash
# Run the real two-process cold-start integration test: it builds both binaries
# (the Go `ccs` control plane and the Rust `ccs-proxy` data plane) and drives
# them as separate processes, asserting cold-start URL minting, warm reuse, and
# proxy-restart port stability. Build-tagged `integration`, so it is excluded
# from the default `go test ./...` CI gate; this script is the convenience
# wrapper for the build-then-test sequence.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

release_tag="v0.0.0-contract.1"
release_version="${release_tag#v}"
release_commit="0123456"
release_dir="$(mktemp -d /tmp/ccs-version-contract.XXXXXX)"
cleanup() {
  rm -rf "$release_dir"
}
trap cleanup EXIT

echo "==> release archive version contract"
CGO_ENABLED=0 go -C "$repo_root/go" build \
  -ldflags "-s -w -X github.com/yasyf/cc-squash/go/internal/version.Version=$release_tag -X github.com/yasyf/cc-squash/go/internal/version.Commit=$release_commit" \
  -o "$release_dir/ccs" ./cmd/ccs
CCS_BUILD_VERSION="$release_version" cargo build -p ccs-proxy \
  --manifest-path "$repo_root/crates/Cargo.toml" \
  --target-dir "$repo_root/target/version-contract"
cp "$repo_root/target/version-contract/debug/ccs-proxy" "$release_dir/ccs-proxy"
tar -czf "$release_dir/release.tar.gz" -C "$release_dir" ccs ccs-proxy
"$repo_root/.github/scripts/verify-release-archive.sh" \
  "$release_tag" "$release_commit" "$release_dir/release.tar.gz"

echo "==> cargo build -p ccs-proxy"
cargo build -p ccs-proxy --manifest-path "$repo_root/crates/Cargo.toml"

echo "==> go test -tags integration ./internal/integration/"
cd "$repo_root/go"
go test -count=1 -tags integration ./internal/integration/ -v "$@"
