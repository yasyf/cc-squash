#!/usr/bin/env bash
set -euo pipefail

workflow=.github/workflows/release.yml
action_pin=19c3d5013032ad9c88f9a8f1170d1f366c19b8d9
draft_pin=54e3e194bda69896894a82c17fcdb2822beefab5

if grep -Eq 'yasyf/homebrew-tap/.+@(v[0-9]+|main)' "$workflow"; then
  echo "homebrew-tap release actions must use an exact commit" >&2
  exit 1
fi
test "$(grep -Ec "uses: yasyf/homebrew-tap/.+@${action_pin}$" "$workflow")" = 4
test "$(grep -Ec "uses: yasyf/homebrew-tap/.+@${draft_pin}$" "$workflow")" = 2
if grep -Eq 'softprops/action-gh-release|/releases/tags/|gh release (view|upload|download|edit)' "$workflow"; then
  echo "release workflow must retain one exact numeric release ID" >&2
  exit 1
fi

for required in \
  'group: release' \
  'name: Verify the exact rendered formula' \
  'name: Upload the verified formula delivery' \
  'name: Record the exact release asset manifest' \
  'name: Stage and verify the complete draft release' \
  'name: Smoke-test the exact downloaded release' \
  "actions/stage-draft-release@${draft_pin}" \
  "actions/publish-draft-release@${draft_pin}" \
  "release-id: \${{ steps.draft.outputs['release-id'] }}" \
  'name: Publish the verified release' \
  'publish-tap:' \
  'name: Download the verified formula delivery' \
  'name: Verify the downloaded formula delivery' \
  'name: Publish the formula to the tap'; do
  grep -Fq "$required" "$workflow"
done

line() { grep -Fn "$1" "$workflow" | cut -d: -f1; }
render="$(line 'name: Render the formula')"
formula="$(line 'name: Verify the exact rendered formula')"
upload="$(line 'name: Upload the verified formula delivery')"
stage="$(line 'name: Stage and verify the complete draft release')"
smoke="$(line 'name: Smoke-test the exact downloaded release')"
publish="$(line 'name: Publish the verified release')"
download="$(line 'name: Download the verified formula delivery')"
tap="$(line 'name: Publish the formula to the tap')"
test "$render" -lt "$formula"
test "$formula" -lt "$upload"
test "$upload" -lt "$stage"
test "$stage" -lt "$smoke"
test "$smoke" -lt "$publish"
test "$publish" -lt "$download"
test "$download" -lt "$tap"
