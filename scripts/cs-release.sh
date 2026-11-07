#!/usr/bin/env bash
# Modified by alfypops, 2026: fork release metadata derives from Cargo.toml
# Modified by alfypops, 2026: cs.3 release notes mention daemon_repo_roots
# Modified by alfypops, 2026: cs.4 release notes mention Bash edits inside the grace window
# Modified by alfypops, 2026: build with plain cargo
set -euo pipefail

publish=false
profile=""
env_file=""
while (($#)); do
  case "$1" in
    --publish) publish=true; shift ;;
    --dry-run) publish=false; shift ;;
    --env-file)
      [[ $# -ge 2 ]] || { echo '--env-file requires a path' >&2; exit 2; }
      env_file=$2; shift 2 ;;
    --keychain-profile)
      [[ $# -ge 2 ]] || { echo '--keychain-profile requires a name' >&2; exit 2; }
      profile=$2; shift 2 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -n "$env_file" ]]; then
  set -a
  . "$env_file"
  set +a
fi
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
export CARGO_TARGET_DIR="$root/target"
export CARGO_BUILD_JOBS=4
version=$(awk -F '"' '
  /^[[:space:]]*\[package\][[:space:]]*$/ { in_package = 1; next }
  /^[[:space:]]*\[/ { in_package = 0 }
  in_package && /^[[:space:]]*version[[:space:]]*=/ {
    if ($0 !~ /^[[:space:]]*version[[:space:]]*=[[:space:]]*"[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?"[[:space:]]*$/) {
      invalid = 1; exit
    }
    version = $2; count++
  }
  END { if (invalid || count != 1) exit 1; print version }
' Cargo.toml) || { echo 'Expected exactly one valid [package] version in Cargo.toml' >&2; exit 1; }
release_notes='Configurable notes_push publication policy and isolated notes_ref; daemon_repo_roots scopes the daemon to listed repositories (trace events, checkpoints and the untraced-commit fixup); Bash edits within the 2 s grace window are recorded and human writes there are never credited to the agent. Apache-2.0. Based on upstream git-ai v1.7.5.'
out="$root/target/cs-release"
binary="$out/git-ai-macos-arm64"

# Display placeholders, never credential values (including keychain names).
step() {
  local label=$1
  shift
  printf '%s\n' "$label"
  if "$publish"; then "$@"; fi
}

if "$publish"; then
  source_commit=$(git rev-parse HEAD)
  : "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
  if [[ -z "$profile" ]]; then
    : "${APPLE_ID:?Set APPLE_ID or use --keychain-profile}"
    : "${APPLE_PASSWORD:?Set APPLE_PASSWORD}"
    : "${APPLE_TEAM_ID:?Set APPLE_TEAM_ID}"
  fi
  available_kb=$(df -Pk "$root" | awk 'END {print $4}')
  (( available_kb >= 45 * 1024 * 1024 )) || { echo 'At least 45 GiB free is required' >&2; exit 1; }
  mkdir -p "$out"
else
  echo 'DRY RUN: commands are displayed only; nothing is built, signed, notarized or uploaded.'
fi
step 'cargo build --release --target aarch64-apple-darwin' \
  cargo build --release --target aarch64-apple-darwin
step 'cp target/aarch64-apple-darwin/release/git-ai target/cs-release/git-ai-macos-arm64' \
  cp target/aarch64-apple-darwin/release/git-ai "$binary"
step 'strip target/cs-release/git-ai-macos-arm64' strip "$binary"
step 'codesign --force --timestamp --options runtime --sign "$APPLE_SIGNING_IDENTITY" target/cs-release/git-ai-macos-arm64' \
  codesign --force --timestamp --options runtime --sign "${APPLE_SIGNING_IDENTITY:-}" "$binary"
step 'cp LICENSE target/cs-release/LICENSE' cp LICENSE "$out/LICENSE"
step 'ditto -c -k --keepParent target/cs-release/git-ai-macos-arm64 target/cs-release/git-ai-macos-arm64.zip' \
  ditto -c -k --keepParent "$binary" "$out/git-ai-macos-arm64.zip"
if [[ -n "$profile" ]]; then
  step 'xcrun notarytool submit target/cs-release/git-ai-macos-arm64.zip --wait --keychain-profile "$KEYCHAIN_PROFILE"' \
    xcrun notarytool submit "$out/git-ai-macos-arm64.zip" --wait --keychain-profile "$profile"
else
  step 'xcrun notarytool submit target/cs-release/git-ai-macos-arm64.zip --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" --wait' \
    xcrun notarytool submit "$out/git-ai-macos-arm64.zip" --apple-id "${APPLE_ID:-}" --password "${APPLE_PASSWORD:-}" --team-id "${APPLE_TEAM_ID:-}" --wait
fi
echo 'codesign -dvv target/cs-release/git-ai-macos-arm64'
if "$publish"; then
  verification=$(codesign -dvv "$binary" 2>&1)
  printf '%s\n' "$verification"
  [[ "$verification" == *'Authority=Developer ID Application:'* && "$verification" == *'(runtime)'* && "$verification" == *'Timestamp='* ]] || {
    echo 'Expected Developer ID authority, runtime flags and a timestamp' >&2
    exit 1
  }
fi
step 'spctl -a -t install target/cs-release/git-ai-macos-arm64' spctl -a -t install "$binary"
echo '(cd target/cs-release && shasum -a 256 git-ai-macos-arm64 > SHA256SUMS)'
if "$publish"; then
  (cd "$out" && shasum -a 256 git-ai-macos-arm64 > SHA256SUMS)
fi
step "gh release create v$version target/cs-release/git-ai-macos-arm64 target/cs-release/SHA256SUMS target/cs-release/LICENSE --repo alfypops/git-ai --target \"\$source_commit\" --title \"alfypops build of git-ai v$version\" --notes \"$release_notes\"" \
  gh release create "v$version" "$binary" "$out/SHA256SUMS" "$out/LICENSE" --repo alfypops/git-ai --target "${source_commit:-}" \
  --title "alfypops build of git-ai v$version" --notes "$release_notes"
