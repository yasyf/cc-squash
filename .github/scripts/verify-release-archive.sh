#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: $0 <v-prefixed-version> <archive>" >&2
  exit 2
fi

tag="$1"
archive="$2"
expected="${tag#v}"
test "$tag" = "v${expected}"
test -n "$expected"

unpack="$(mktemp -d /tmp/cc-squash-release-version.XXXXXX)"
cleanup() {
  rm -rf "$unpack"
}
trap cleanup EXIT

contents="$(tar -tzf "$archive" | LC_ALL=C sort)"
test "$contents" = $'ccs\nccs-proxy'
tar -xzf "$archive" -C "$unpack"

control_output="$("$unpack/ccs" --version)"
proxy_output="$("$unpack/ccs-proxy" --version)"

if [[ "$control_output" != "$tag" || "$proxy_output" != "ccs-proxy $expected" ]]; then
  echo "release version mismatch: tag=$tag ccs=$control_output ccs-proxy=$proxy_output" >&2
  exit 1
fi
