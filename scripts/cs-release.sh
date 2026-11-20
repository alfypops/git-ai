#!/usr/bin/env bash
# Modified by alfypops, 2026: fork release metadata derives from Cargo.toml
# Modified by alfypops, 2026: cs.3 release notes mention daemon_repo_roots
# Modified by alfypops, 2026: cs.4 release notes mention Bash edits inside the grace window
# Modified by alfypops, 2026: build with plain cargo
# Modified by alfypops, 2026: CI creates the release; add the macOS binary to it
# Modified by alfypops, 2026: CI drafts the release; add the macOS binary, verify, then publish
set -euo pipefail

# Display placeholders, never credential values (including keychain names).
# A failed step stops the run even where set -e does not apply (the tests source this file).
step() {
  local label=$1
  shift
  printf '%s\n' "$label"
  if "$publish"; then "$@" || exit; fi
}

check_tag() {
  local status=0
  git ls-remote --exit-code --tags "$remote_url" "refs/tags/v$version" > /dev/null || status=$?
  if ((status == 2)); then
    echo "Tag v$version is not pushed, so release v$version does not exist: push the tag and let CI create the draft release first" >&2
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

# Every release for v$version, drafts included, one per line: id, draft, author, created.
# GitHub allows several drafts for one tag, and gh's tag lookup then returns whichever it
# finds first, so the script works by id once it has found the one release.
release_ids() {
  gh api --paginate "repos/$repo/releases" \
    --jq ".[] | select(.tag_name == \"v$version\") | \"\\(.id) draft=\\(.draft) \\(.author.login) \\(.created_at)\""
}

# How to delete one release whatever its tag lookup would pick.
delete_hint() {
  printf 'Delete a release by its id: gh api -X DELETE repos/%s/releases/<id>\n' "$repo"
}

# CI's tag run creates the draft only when no release exists for the tag.
check_no_release() {
  local ids
  ids=$(release_ids) || { echo "Could not list the releases on $repo" >&2; exit 1; }
  [[ -z $ids ]] || {
    printf 'A release for v%s already exists on %s:\n%s\n' "$version" "$repo" "$ids" >&2
    echo 'If it is a draft an earlier run left, delete it and re-run; a published release is immutable, so bump Cargo.toml and push a new tag.' >&2
    delete_hint >&2
    exit 1
  }
}

# Prints the id of the one release for v$version.
the_release() {
  local ids count
  ids=$(release_ids) || { echo "Could not list the releases on $repo" >&2; return 1; }
  count=$(grep -c . <<< "$ids") || true
  case $count in
    0) echo "Release v$version on $repo does not exist; push the tag v$version and let CI create the draft first" >&2; return 1 ;;
    1) awk '{print $1}' <<< "$ids" ;;
    *)
      printf 'Found %s releases for v%s on %s; delete all but CI'"'"'s draft, then re-run:\n%s\n' "$count" "$version" "$repo" "$ids" >&2
      delete_hint >&2
      return 1
      ;;
  esac
}

# Every gh step after check_release checks it still works on the release check_release found.
same_release() {
  [[ $(the_release) == "$release_id" ]] ||
    { echo "The release for v$version on $repo changed during this run (it was id $release_id); start again" >&2; exit 1; }
}

# The release holds exactly LICENSE (the tag's copy), SHA256SUMS (the same as $1, five
# lines) and the five binaries it lists, and every binary matches its line.
check_release_assets() {
  local sums=$1 dir="$out/release" names expected
  [[ $(wc -l < "$sums") -eq 5 ]] || return 1
  names=$(gh release view "v$version" --repo "$repo" --json assets --jq '.assets[].name' | sort) || return 1
  expected=$({ printf '%s\n' LICENSE SHA256SUMS; awk '{print $2}' "$sums"; } | sort)
  [[ $names == "$expected" ]] || { printf 'Release assets:\n%s\nExpected:\n%s\n' "$names" "$expected" >&2; return 1; }
  rm -rf "$dir"
  gh release download "v$version" --repo "$repo" --dir "$dir" &&
    cmp "$dir/SHA256SUMS" "$sums" && cmp "$dir/LICENSE" "$root/LICENSE" &&
    (cd "$dir" && shasum -a 256 -c SHA256SUMS)
}

# Published releases are immutable, so the macOS binary can only be added to CI's draft.
# A published release ends the run: complete if an earlier run stopped after publishing it,
# else it can never be fixed.
check_release() {
  local is_draft
  gh auth status --hostname github.com > /dev/null || { echo 'gh is not authenticated; run gh auth login' >&2; exit 1; }
  release_id=$(the_release) || exit 1
  is_draft=$(gh release view "v$version" --repo "$repo" --json isDraft --jq .isDraft) ||
    { echo "Release v$version on $repo could not be read (see gh's error above)" >&2; exit 1; }
  [[ $is_draft == true ]] && return
  rm -rf "$out/published"
  if gh release download "v$version" --repo "$repo" --pattern SHA256SUMS --dir "$out/published" &&
    grep -q ' git-ai-macos-arm64$' "$out/published/SHA256SUMS" &&
    check_release_assets "$out/published/SHA256SUMS"; then
    echo "Release v$version is already published and complete: its five binaries match its SHA256SUMS. Nothing to do."
    exit 0
  fi
  echo "Release v$version on $repo is published but incomplete, and published releases are immutable; bump the version in Cargo.toml and cut a new release" >&2
  exit 1
}

download_release_assets() {
  rm -rf "$assets"
  mkdir -p "$assets"
  gh release download "v$version" --repo "$repo" --pattern 'git-ai-*' --pattern SHA256SUMS --dir "$assets"
}

# The draft's SHA256SUMS lists CI's four binaries, plus macOS after an earlier run's upload.
# A re-run replaces any macOS asset and line on the draft; CI's four lines must still match.
verify_ci_binaries() {
  if [[ ! -f "$assets/SHA256SUMS" ]]; then
    cat >&2 << EOF
Draft v$version has no SHA256SUMS (an earlier upload removed it). Restore CI's copy, then re-run:
  gh run download <tag run id> --repo $repo -n SHA256SUMS
  gh release upload v$version SHA256SUMS --repo $repo
The tag run keeps its artifacts for 7 days; after that, delete the draft
(gh api -X DELETE repos/$repo/releases/$release_id) and re-run the tag run.
GitHub re-runs a run only within 30 days of the tag run; after that, cut a new version.
EOF
    exit 1
  fi
  (cd "$assets" && grep -v ' git-ai-macos-arm64$' SHA256SUMS > ci-SHA256SUMS &&
    [[ $(wc -l < ci-SHA256SUMS) -eq 4 ]] && shasum -a 256 -c ci-SHA256SUMS) ||
    {
      echo "CI's binaries on v$version do not match its SHA256SUMS. Do not regenerate SHA256SUMS: it would vouch for whatever is on the draft." >&2
      echo "Delete the draft (gh api -X DELETE repos/$repo/releases/$release_id) and re-run the tag run, or cut a new version." >&2
      exit 1
    }
}

write_sums() {
  (cd "$assets" && shasum -a 256 git-ai-* > ../SHA256SUMS)
  cat "$out/SHA256SUMS"
  [[ $(wc -l < "$out/SHA256SUMS") -eq 5 ]] || { echo 'Expected SHA256SUMS over 5 binaries' >&2; exit 1; }
}

# --clobber replaces CI's SHA256SUMS, and a macOS binary an earlier run left on the draft.
upload_macos() {
  same_release
  gh release upload "v$version" "$binary" "$out/SHA256SUMS" --repo "$repo" --clobber
}

verify_draft() {
  same_release
  check_release_assets "$out/SHA256SUMS" ||
    { echo "The assets on draft v$version do not match what this run checked and uploaded; nothing was published" >&2; exit 1; }
}

# Publishes the pinned release by id, never by a tag lookup.
publish_release() {
  local draft
  same_release
  draft=$(gh api -X PATCH "repos/$repo/releases/$release_id" -F draft=false --jq .draft) ||
    { echo "Publishing release $release_id (v$version) failed (see gh's error above); re-run to retry" >&2; exit 1; }
  [[ $draft == false ]] ||
    { echo "Release $release_id (v$version) is still a draft after publishing; re-run to retry" >&2; exit 1; }
}

verify_published() {
  same_release
  check_release_assets "$out/SHA256SUMS" || {
    echo "Published v$version does not match what this run uploaded, and published releases are immutable: say so in its notes and cut a new version" >&2
    exit 1
  }
}

# Before the long build: CI's draft is the only release for the tag, and its binaries match.
check_draft() {
  step "gh auth status; gh api repos/$repo/releases; gh release view v$version --json isDraft (refuse unless CI's draft is the only release for v$version; a complete published release ends the run)" \
    check_release
  step "gh release download v$version --repo $repo --pattern 'git-ai-*' --pattern SHA256SUMS --dir target/cs-release/assets" \
    download_release_assets
  step "(cd target/cs-release/assets && shasum -a 256 -c CI's four SHA256SUMS lines)" \
    verify_ci_binaries
}

# After signing: SHA256SUMS covers CI's four binaries plus macOS. Only a verified draft is published.
add_macos_and_publish() {
  local checks="(exactly LICENSE, SHA256SUMS and its 5 binaries; cmp SHA256SUMS and LICENSE; shasum -a 256 -c)"
  step 'cp target/cs-release/git-ai-macos-arm64 target/cs-release/assets/git-ai-macos-arm64' cp "$binary" "$assets/"
  step '(cd target/cs-release/assets && shasum -a 256 git-ai-* > ../SHA256SUMS) (expect 5 lines)' write_sums
  step "gh release upload v$version target/cs-release/git-ai-macos-arm64 target/cs-release/SHA256SUMS --repo $repo --clobber" \
    upload_macos
  step "gh release view/download v$version: verify the draft $checks" verify_draft
  step "gh api -X PATCH repos/$repo/releases/<id> -F draft=false (publish the pinned release; refuse unless it is no longer a draft)" publish_release
  step "gh release view/download v$version: verify the published release $checks" verify_published
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
# The draft release and its notes (cs-release-notes.md) come from CI's tag run; this script
# adds the macOS binary to that draft and publishes it.
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
  echo 'DRY RUN: commands are displayed only; nothing is built, signed, notarized, uploaded or published.'
fi
step "git ls-remote/fetch https://github.com/$repo.git tag v$version (refuse unless it is pushed, HEAD is v$version and the tree is clean)" check_tag
check_draft
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
add_macos_and_publish
