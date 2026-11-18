#!/usr/bin/env bash
# Modified by alfypops, 2026: fork release metadata derives from Cargo.toml
# Modified by alfypops, 2026: cs.3 release notes mention daemon_repo_roots
# Modified by alfypops, 2026: cs.4 release notes mention Bash edits inside the grace window
# Modified by alfypops, 2026: build with plain cargo
# Modified by alfypops, 2026: CI creates the release; add the macOS binary to it
set -euo pipefail

# Display placeholders, never credential values (including keychain names).
step() {
  local label=$1
  shift
  printf '%s\n' "$label"
  if "$publish"; then "$@"; fi
}

check_tag() {
  local status=0
  git ls-remote --exit-code --tags "$remote_url" "refs/tags/v$version" > /dev/null || status=$?
  if ((status == 2)); then
    echo "Tag v$version is not pushed, so release v$version does not exist: push the tag and let CI create the release first" >&2
    exit 1
  fi
  ((status == 0)) || { echo "Could not read tags from $repo" >&2; exit 1; }
  git fetch --quiet "$remote_url" "refs/tags/v$version:refs/tags/v$version" ||
    { echo "Could not fetch tag v$version from $repo" >&2; exit 1; }
  [[ $(git rev-parse HEAD) == $(git rev-parse "v$version^{commit}") ]] ||
    { echo "HEAD is not the commit tagged v$version; check out v$version first" >&2; exit 1; }
  [[ -z $(git status --porcelain) ]] ||
    { echo 'The working tree has uncommitted or untracked files' >&2; exit 1; }
}

check_release() {
  gh auth status --hostname github.com > /dev/null || { echo 'gh is not authenticated; run gh auth login' >&2; exit 1; }
  gh release view "v$version" --repo "$repo" > /dev/null ||
    { echo "Release v$version on $repo could not be read (see gh's error above); if it does not exist, push the tag v$version and let CI create it first" >&2; exit 1; }
}

download_release_assets() {
  rm -rf "$assets"
  mkdir -p "$assets"
  gh release download "v$version" --repo "$repo" --pattern 'git-ai-*' --pattern SHA256SUMS --dir "$assets"
}

# The release's SHA256SUMS lists CI's four binaries, plus macOS once it is published.
# A macOS asset without its SHA256SUMS line is a partial publish, which a re-run replaces;
# so is a macOS line whose asset is missing or does not match it (CI's four lines still must).
verify_ci_binaries() {
  if [[ ! -f "$assets/SHA256SUMS" ]]; then
    cat >&2 << EOF
Release v$version has no SHA256SUMS (an earlier upload removed it). Restore CI's copy, then re-run:
  gh run download <tag run id> --repo $repo -n SHA256SUMS
  gh release upload v$version SHA256SUMS --repo $repo
The tag run keeps its artifacts for 7 days; after that, run gh release delete v$version and re-run the tag run.
GitHub re-runs a run only within 30 days of it; after that, cut a new version.
EOF
    exit 1
  fi
  if [[ -f "$assets/git-ai-macos-arm64" ]] &&
    (cd "$assets" && grep ' git-ai-macos-arm64$' SHA256SUMS > macos-SHA256SUMS &&
      shasum -a 256 -c macos-SHA256SUMS > /dev/null 2>&1); then
    echo "v$version already has a published macOS binary; refusing to replace it" >&2
    exit 1
  fi
  (cd "$assets" && grep -v ' git-ai-macos-arm64$' SHA256SUMS > ci-SHA256SUMS &&
    [[ $(wc -l < ci-SHA256SUMS) -eq 4 ]] && shasum -a 256 -c ci-SHA256SUMS) ||
    { echo "CI's binaries on v$version do not match its SHA256SUMS" >&2; exit 1; }
}

write_sums() {
  (cd "$assets" && shasum -a 256 git-ai-* > ../SHA256SUMS)
  cat "$out/SHA256SUMS"
  [[ $(wc -l < "$out/SHA256SUMS") -eq 5 ]] || { echo 'Expected SHA256SUMS over 5 binaries' >&2; exit 1; }
}

verify_upload() {
  local uploaded="$out/uploaded"
  rm -rf "$uploaded"
  gh release download "v$version" --repo "$repo" --pattern git-ai-macos-arm64 --pattern SHA256SUMS --dir "$uploaded"
  { cmp "$uploaded/SHA256SUMS" "$out/SHA256SUMS" &&
    (cd "$uploaded" && grep ' git-ai-macos-arm64$' SHA256SUMS > macos-SHA256SUMS && shasum -a 256 -c macos-SHA256SUMS); } ||
    { echo "The uploaded macOS binary or SHA256SUMS on v$version does not match" >&2; exit 1; }
}

# scripts/tests/cs-release-test.sh sources this file for the functions above.
[[ "${BASH_SOURCE[0]}" == "$0" ]] || return 0

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
remote_url="https://github.com/$repo.git"
out="$root/target/cs-release"
binary="$out/git-ai-macos-arm64"
assets="$out/assets"

if "$publish"; then
  : "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY}"
  if [[ -z "$profile" ]]; then
    : "${APPLE_ID:?Set APPLE_ID or use --keychain-profile}"
    : "${APPLE_PASSWORD:?Set APPLE_PASSWORD}"
    : "${APPLE_TEAM_ID:?Set APPLE_TEAM_ID}"
  fi
  available_kb=$(df -Pk "$root" | awk 'END {print $4}')
  (( available_kb >= 45 * 1024 * 1024 )) || { echo 'At least 45 GiB free is required' >&2; exit 1; }
else
  echo 'DRY RUN: commands are displayed only; nothing is built, signed, notarized or uploaded.'
fi
step "git ls-remote/fetch https://github.com/$repo.git tag v$version (refuse unless it is pushed, HEAD is v$version and the tree is clean)" check_tag
step "gh auth status; gh release view v$version --repo $repo (refuse unless CI already created the release)" check_release
# Check the release's assets before the long build: CI's binaries must match its
# SHA256SUMS, and a release that already has the macOS binary is refused.
step "gh release download v$version --repo $repo --pattern 'git-ai-*' --pattern SHA256SUMS --dir target/cs-release/assets" \
  download_release_assets
step "(cd target/cs-release/assets && shasum -a 256 -c CI's four SHA256SUMS lines; refuse if macOS is already published)" \
  verify_ci_binaries
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
step 'cp target/cs-release/git-ai-macos-arm64 target/cs-release/assets/git-ai-macos-arm64' cp "$binary" "$assets/"
step '(cd target/cs-release/assets && shasum -a 256 git-ai-* > ../SHA256SUMS) (expect 5 lines)' write_sums
# --clobber replaces CI's SHA256SUMS, and the macOS binary only after a partial publish:
# verify_ci_binaries refused a release whose macOS binary is already published.
step "gh release upload v$version target/cs-release/git-ai-macos-arm64 target/cs-release/SHA256SUMS --repo $repo --clobber" \
  gh release upload "v$version" "$binary" "$out/SHA256SUMS" --repo "$repo" --clobber
step "gh release download v$version --pattern git-ai-macos-arm64 --pattern SHA256SUMS --dir target/cs-release/uploaded (compare, shasum -a 256 -c)" \
  verify_upload
