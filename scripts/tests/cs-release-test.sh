#!/usr/bin/env bash
# Tests scripts/cs-release.sh's release checks with a gh shim and temp dirs; nothing is
# built, signed or uploaded.
set -euo pipefail

script="$(cd "$(dirname "$0")/.." && pwd)/cs-release.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# gh shim: the release is the directory $FAKE_RELEASE.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" << 'SHIM'
#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  'auth status') exit 0 ;;
  'release view') [[ -d $FAKE_RELEASE ]] ;;
  'release download')
    shift 3
    patterns=() dir=.
    while (($#)); do
      case "$1" in
        --pattern) patterns+=("$2"); shift 2 ;;
        --dir) dir=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    mkdir -p "$dir"
    for file in "$FAKE_RELEASE"/*; do
      for pattern in "${patterns[@]}"; do
        # shellcheck disable=SC2053 # glob match is intended
        if [[ $(basename "$file") == $pattern ]]; then cp "$file" "$dir/"; fi
      done
    done
    ;;
  'release upload')
    shift 3
    for file in "$@"; do [[ $file == --* || ! -f $file ]] || cp "$file" "$FAKE_RELEASE/"; done
    ;;
  *) echo "gh shim: unexpected $*" >&2; exit 1 ;;
esac
SHIM
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH"

# shellcheck source=scripts/cs-release.sh
. "$script"
set +e
version=1.0.0-test
repo=example/git-ai
out="$tmp/out"
assets="$out/assets"
binary="$out/git-ai-macos-arm64"
export FAKE_RELEASE="$tmp/release"

failures=0
# expect <pass|refuse> <description> <expected stderr text or ''> <command...>
expect() {
  local want=$1 name=$2 text=$3 output status
  shift 3
  output=$( ("$@") 2>&1)
  status=$?
  if [[ $want == pass && $status -eq 0 ]] ||
    [[ $want == refuse && $status -ne 0 && $output == *"$text"* ]]; then
    echo "ok   - $name"
  else
    echo "FAIL - $name (exit $status)"
    printf '%s\n' "$output" | sed 's/^/       /'
    failures=$((failures + 1))
  fi
}

new_release() {
  rm -rf "$FAKE_RELEASE" "$out"
  mkdir -p "$FAKE_RELEASE" "$out"
  for name in linux-x86_64 linux-arm64 windows-x86_64.exe windows-arm64.exe; do
    echo "ci $name" > "$FAKE_RELEASE/git-ai-$name"
  done
  (cd "$FAKE_RELEASE" && shasum -a 256 git-ai-* > SHA256SUMS)
}

publish_macos() {
  echo "macos $1" > "$binary"
  download_release_assets && verify_ci_binaries && cp "$binary" "$assets/" && write_sums &&
    gh release upload "v$version" "$binary" "$out/SHA256SUMS" --repo "$repo" --clobber && verify_upload
}

new_release
expect pass 'fresh release: publish macOS and verify the upload' '' publish_macos one
expect refuse 'complete publish: a re-run is refused' 'already has a published macOS binary' publish_macos two
grep -q 'macos one' "$FAKE_RELEASE/git-ai-macos-arm64" || { echo 'FAIL - published macOS binary changed'; failures=$((failures + 1)); }

new_release
echo 'macos partial' > "$FAKE_RELEASE/git-ai-macos-arm64"
expect pass 'partial publish (macOS asset, 4-line SHA256SUMS): a re-run replaces both' '' publish_macos three
# shellcheck disable=SC2016 # expanded by the inner shell
expect pass 'partial re-run left a consistent release' '' \
  bash -c 'cd "$FAKE_RELEASE" && [[ $(wc -l < SHA256SUMS) -eq 5 ]] && shasum -a 256 -c SHA256SUMS'

new_release
rm "$FAKE_RELEASE/SHA256SUMS"
expect refuse 'missing SHA256SUMS is refused with recovery steps' 'gh run download <tag run id>' publish_macos four

new_release
echo tampered > "$FAKE_RELEASE/git-ai-linux-arm64"
expect refuse 'tampered CI binary is refused' "CI's binaries on v$version do not match" publish_macos five

expect pass 'existing release passes check_release' '' check_release
rm -rf "$FAKE_RELEASE"
expect refuse 'missing release is refused, naming it' "Release v$version on $repo could not be read" check_release

new_release
expect pass 'verify_upload setup: publish macOS' '' publish_macos six
echo tampered > "$FAKE_RELEASE/git-ai-macos-arm64"
expect refuse 'verify_upload: changed macOS binary is refused' 'uploaded macOS binary or SHA256SUMS' verify_upload
expect pass 'verify_upload setup: re-publish over the changed binary' '' publish_macos seven
echo extra >> "$FAKE_RELEASE/SHA256SUMS"
expect refuse 'verify_upload: changed SHA256SUMS is refused' 'uploaded macOS binary or SHA256SUMS' verify_upload

new_release
grep -v ' git-ai-linux-arm64$' "$FAKE_RELEASE/SHA256SUMS" > "$tmp/sums" && mv "$tmp/sums" "$FAKE_RELEASE/SHA256SUMS"
expect refuse 'SHA256SUMS with 3 non-macOS lines is refused' "CI's binaries on v$version do not match" publish_macos eight

new_release
echo 'ci extra' > "$FAKE_RELEASE/git-ai-extra"
(cd "$FAKE_RELEASE" && shasum -a 256 git-ai-extra >> SHA256SUMS)
expect refuse 'SHA256SUMS with 5 non-macOS lines is refused' "CI's binaries on v$version do not match" publish_macos nine

# A macOS line that does not match its asset, or whose asset is missing, is a partial publish.
new_release
echo 'macos old' > "$FAKE_RELEASE/git-ai-macos-arm64"
(cd "$FAKE_RELEASE" && shasum -a 256 git-ai-macos-arm64 >> SHA256SUMS)
echo 'macos changed' > "$FAKE_RELEASE/git-ai-macos-arm64"
expect pass 'macOS line not matching its asset: a re-run replaces both' '' publish_macos ten
new_release
(cd "$FAKE_RELEASE" && echo 'macos gone' > git-ai-macos-arm64 && shasum -a 256 git-ai-macos-arm64 >> SHA256SUMS && rm git-ai-macos-arm64)
expect pass 'macOS line with no asset: a re-run replaces it' '' publish_macos eleven

# The dry run prints the steps in order: verify CI's binaries before the build, upload after the sums.
step_line() { printf '%s\n' "$dry_run" | grep -n -m1 -F -- "$1" | cut -d: -f1; }
dry_run=$(bash "$script" --dry-run)
verify_line=$(step_line "shasum -a 256 -c CI's four")
build_line=$(step_line 'cargo build --release')
sums_line=$(step_line '(expect 5 lines)')
upload_line=$(step_line 'gh release upload')
if [[ -n $verify_line && -n $build_line && -n $sums_line && -n $upload_line ]] &&
  ((verify_line < build_line && sums_line < upload_line)); then
  echo 'ok   - dry run orders verification, build, sums and upload'
else
  echo "FAIL - dry run step order (verify $verify_line, build $build_line, sums $sums_line, upload $upload_line)"
  failures=$((failures + 1))
fi

# check_tag against a local stand-in for the fork.
git init -q --bare "$tmp/fork.git"
git init -q "$tmp/clone"
cd "$tmp/clone"
git -c user.name=test -c user.email=test@example.com commit -q --allow-empty -m tagged
remote_url="$tmp/fork.git"
expect refuse 'unpushed tag is refused, naming the missing release' "release v$version does not exist" check_tag
git tag "v$version"
git push -q "$remote_url" "v$version"
git tag -d "v$version" > /dev/null
expect pass 'HEAD at the pushed tag with a clean tree' '' check_tag
touch untracked
expect refuse 'untracked file is refused' 'uncommitted or untracked' check_tag
rm untracked
echo change > tracked
git add tracked
expect refuse 'staged change is refused' 'uncommitted or untracked' check_tag
git -c user.name=test -c user.email=test@example.com commit -q -m later
expect refuse 'HEAD past the tag is refused' "HEAD is not the commit tagged v$version" check_tag

remote_url="$tmp/missing.git"
expect refuse 'unreadable remote is refused' "Could not read tags from $repo" check_tag

((failures == 0)) || { echo "$failures failed"; exit 1; }
echo 'all passed'
