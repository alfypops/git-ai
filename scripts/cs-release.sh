#!/usr/bin/env bash
# Modified by alfypops, 2026: fork release metadata derives from Cargo.toml
# Modified by alfypops, 2026: cs.3 release notes mention daemon_repo_roots
# Modified by alfypops, 2026: cs.4 release notes mention Bash edits inside the grace window
# Modified by alfypops, 2026: build with plain cargo
# Modified by alfypops, 2026: CI creates the release; add the macOS binary to it
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
# The release and its notes (cs-release-notes.md) come from CI's tag run; this script
# only adds the macOS binary to that release.
[[ -s cs-release-notes.md ]] || { echo 'Expected release notes in cs-release-notes.md' >&2; exit 1; }
repo=alfypops/git-ai
out="$root/target/cs-release"
binary="$out/git-ai-macos-arm64"
assets="$out/assets"

# Display placeholders, never credential values (including keychain names).
step() {
  local label=$1
  shift
  printf '%s\n' "$label"
  if "$publish"; then "$@"; fi
}

if "$publish"; then
  : "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
  if [[ -z "$profile" ]]; then
    : "${APPLE_ID:?Set APPLE_ID or use --keychain-profile}"
    : "${APPLE_PASSWORD:?Set APPLE_PASSWORD}"
    : "${APPLE_TEAM_ID:?Set APPLE_TEAM_ID}"
  fi
  available_kb=$(df -Pk "$root" | awk 'END {print $4}')
  (( available_kb >= 45 * 1024 * 1024 )) || { echo 'At least 45 GiB free is required' >&2; exit 1; }
  gh release view "v$version" --repo "$repo" > /dev/null 2>&1 || {
    echo "Release v$version does not exist on $repo; push the tag v$version and let CI create it first" >&2
    exit 1
  }
  rm -rf "$assets"
  mkdir -p "$out" "$assets"
else
  echo 'DRY RUN: commands are displayed only; nothing is built, signed, notarized or uploaded.'
fi
echo "gh release view v$version --repo $repo (refuse unless CI already created the release)"
step 'cargo build --release --target aarch64-apple-darwin' \
  cargo build --release --target aarch64-apple-darwin
step 'cp target/aarch64-apple-darwin/release/git-ai target/cs-release/git-ai-macos-arm64' \
  cp target/aarch64-apple-darwin/release/git-ai "$binary"
step 'strip target/cs-release/git-ai-macos-arm64' strip "$binary"
step 'codesign --force --timestamp --options runtime --sign "$APPLE_SIGNING_IDENTITY" target/cs-release/git-ai-macos-arm64' \
  codesign --force --timestamp --options runtime --sign "${APPLE_SIGNING_IDENTITY:-}" "$binary"
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
# SHA256SUMS covers every binary on the release: CI's Linux and Windows builds plus macOS.
step "gh release download v$version --repo $repo --pattern 'git-ai-*' --dir target/cs-release/assets" \
  gh release download "v$version" --repo "$repo" --pattern 'git-ai-*' --dir "$assets"
step 'cp target/cs-release/git-ai-macos-arm64 target/cs-release/assets/git-ai-macos-arm64' cp "$binary" "$assets/"
echo '(cd target/cs-release/assets && shasum -a 256 git-ai-* > ../SHA256SUMS)'
if "$publish"; then
  (cd "$assets" && shasum -a 256 git-ai-* > ../SHA256SUMS)
  cat "$out/SHA256SUMS"
fi
step "gh release upload v$version target/cs-release/git-ai-macos-arm64 --repo $repo" \
  gh release upload "v$version" "$binary" --repo "$repo"
step "gh release upload v$version target/cs-release/SHA256SUMS --repo $repo --clobber" \
  gh release upload "v$version" "$out/SHA256SUMS" --repo "$repo" --clobber
