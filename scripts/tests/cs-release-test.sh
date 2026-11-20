#!/usr/bin/env bash
# Tests scripts/cs-release.sh's release checks with a gh shim and temp dirs; nothing is
# built, signed or uploaded. The release steps run through the script's own check_draft
# and add_macos_and_publish, so the tests follow its real step order.
set -euo pipefail

script="$(cd "$(dirname "$0")/.." && pwd)/cs-release.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# gh shim: the release is the directory $FAKE_RELEASE, a draft until $FAKE_RELEASE.published
# exists. $FAKE_RELEASE.ids lists the ids of the releases for its tag (default one, 41).
# Every call is appended to $FAKE_LOG. FAKE_AFTER_UPLOAD and FAKE_AFTER_EDIT are shell
# snippets run after an upload or a publish; FAKE_EDIT_NOOP=1 makes release edit do nothing.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/gh" << 'SHIM'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$FAKE_LOG"
case "$1 $2" in
  'auth status') exit 0 ;;
  'api --paginate')
    [[ -d $FAKE_RELEASE ]] || exit 0
    if [[ -f $FAKE_RELEASE.ids ]]; then cat "$FAKE_RELEASE.ids"; else echo 41; fi
    ;;
  'release view')
    [[ -d $FAKE_RELEASE ]]
    case "$*" in
      *'--json isDraft'*) if [[ -f $FAKE_RELEASE.published ]]; then echo false; else echo true; fi ;;
      *'--json assets'*) for file in "$FAKE_RELEASE"/*; do basename "$file"; done ;;
    esac
    ;;
  'release edit')
    [[ $* == *'--draft=false'* ]] || { echo "gh shim: unexpected $*" >&2; exit 1; }
    [[ ${FAKE_EDIT_NOOP:-} == 1 ]] || touch "$FAKE_RELEASE.published"
    bash -c "${FAKE_AFTER_EDIT:-}"
    ;;
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
    ((${#patterns[@]})) || patterns=('*')
    mkdir -p "$dir"
    for file in "$FAKE_RELEASE"/*; do
      for pattern in "${patterns[@]}"; do
        # shellcheck disable=SC2053 # glob match is intended
        if [[ $(basename "$file") == $pattern ]]; then cp "$file" "$dir/"; fi
      done
    done
    ;;
  'release upload')
    [[ ! -f $FAKE_RELEASE.published ]] || { echo 'HTTP 422: Cannot delete asset from an immutable release' >&2; exit 1; }
    shift 3
    for file in "$@"; do [[ $file == --* || ! -f $file ]] || cp "$file" "$FAKE_RELEASE/"; done
    bash -c "${FAKE_AFTER_UPLOAD:-}"
    ;;
  *) echo "gh shim: unexpected $*" >&2; exit 1 ;;
esac
SHIM
chmod +x "$tmp/bin/gh"
export PATH="$tmp/bin:$PATH"

# shellcheck source=scripts/cs-release.sh
. "$script"
set +e
publish=true
version=1.0.0-test
repo=example/git-ai
root="$tmp/root"
out="$tmp/out"
assets="$out/assets"
binary="$out/git-ai-macos-arm64"
export FAKE_RELEASE="$tmp/release" FAKE_LOG="$tmp/gh.log"
mkdir -p "$root"
echo 'license text' > "$root/LICENSE"

failures=0
fail() {
  echo "FAIL - $1"
  failures=$((failures + 1))
}
# expect <pass|refuse> <description> <expected output text or ''> <command...>
expect() {
  local want=$1 name=$2 text=$3 output status
  shift 3
  output=$( ("$@") 2>&1)
  status=$?
  if [[ $want == pass && $status -eq 0 && $output == *"$text"* ]] ||
    [[ $want == refuse && $status -ne 0 && $output == *"$text"* ]]; then
    echo "ok   - $name"
  else
    fail "$name (exit $status)"
    printf '%s\n' "$output" | sed 's/^/       /'
  fi
}

new_release() {
  rm -rf "$FAKE_RELEASE" "$FAKE_RELEASE.published" "$FAKE_RELEASE.ids" "$out" "$FAKE_LOG"
  mkdir -p "$FAKE_RELEASE" "$out"
  for name in linux-x86_64 linux-arm64 windows-x86_64.exe windows-arm64.exe; do
    echo "ci $name" > "$FAKE_RELEASE/git-ai-$name"
  done
  (cd "$FAKE_RELEASE" && shasum -a 256 git-ai-* > SHA256SUMS)
  cp "$root/LICENSE" "$FAKE_RELEASE/"
}

publish_macos() {
  echo "macos $1" > "$binary"
  check_draft && add_macos_and_publish
}
published() { [[ -f $FAKE_RELEASE.published ]]; }
published_and_consistent() {
  published && (cd "$FAKE_RELEASE" && [[ $(wc -l < SHA256SUMS) -eq 5 ]] && shasum -a 256 -c SHA256SUMS > /dev/null)
}
no_publish_attempt() { ! grep -q '^release edit' "$FAKE_LOG"; }

# CI's tag run: check_no_release before creating the draft.
rm -rf "$FAKE_RELEASE" "$FAKE_RELEASE.ids"
expect pass 'tag run: no release for the tag passes' '' check_no_release
new_release
expect refuse 'tag run: an existing draft is refused with the way out' 'gh release delete v1.0.0-test' check_no_release
touch "$FAKE_RELEASE.published"
expect refuse 'tag run: a published release is refused' 'bump Cargo.toml and push a new tag' check_no_release
printf '41\n42\n' > "$FAKE_RELEASE.ids"
expect refuse 'tag run: two releases are refused' 'already exists' check_no_release

new_release
expect pass 'fresh draft: add macOS, verify, publish, verify' '' publish_macos one
published_and_consistent || fail 'fresh draft ends published and consistent'
expect pass 'complete published release: a re-run ends with nothing to do' 'Nothing to do' publish_macos two
grep -q 'macos one' "$FAKE_RELEASE/git-ai-macos-arm64" || fail 'published macOS binary changed'

new_release
touch "$FAKE_RELEASE.published"
expect refuse 'published release without macOS is refused as immutable' 'published but incomplete' publish_macos three

new_release
echo 'macos partial' > "$FAKE_RELEASE/git-ai-macos-arm64"
expect pass 'draft with a partial upload (macOS asset, 4-line SHA256SUMS): a re-run replaces both' '' publish_macos four
published_and_consistent || fail 'partial re-run left a published, consistent release'

new_release
FAKE_EDIT_NOOP=1 expect refuse 'publish that leaves a draft is refused' 'still a draft' publish_macos five
expect pass 'after a publish that left a draft, a re-run replaces macOS and publishes' '' publish_macos six
if ! { grep -q 'macos six' "$FAKE_RELEASE/git-ai-macos-arm64" && published_and_consistent; }; then
  fail 're-run did not replace the draft macOS binary and publish'
fi

new_release
rm "$FAKE_RELEASE/SHA256SUMS"
expect refuse 'missing SHA256SUMS is refused with recovery steps' 'gh run download <tag run id>' publish_macos seven

new_release
echo tampered > "$FAKE_RELEASE/git-ai-linux-arm64"
expect refuse 'tampered CI binary is refused' "CI's binaries on v$version do not match" publish_macos eight

new_release
printf '41\n42\n' > "$FAKE_RELEASE.ids"
expect refuse 'two releases for the tag are refused' "Found 2 releases for v$version" publish_macos nine
touch "$FAKE_RELEASE.published"
expect refuse 'a published release and a draft for the tag are refused' "Found 2 releases for v$version" publish_macos ten
rm -rf "$FAKE_RELEASE" "$FAKE_RELEASE.ids"
expect refuse 'missing release is refused, naming it' "Release v$version on $repo does not exist" publish_macos eleven

# Anything that changes on the draft after the upload stops the run before the publish.
# shellcheck disable=SC2016 # expanded by the shim's shell
for change in \
  'echo tampered > "$FAKE_RELEASE/git-ai-macos-arm64"' \
  'echo tampered > "$FAKE_RELEASE/git-ai-linux-x86_64"' \
  'cp "$FAKE_RELEASE/git-ai-linux-x86_64" "$FAKE_RELEASE/git-ai-extra"' \
  'echo extra > "$FAKE_RELEASE/notes.txt"' \
  'rm "$FAKE_RELEASE/LICENSE"' \
  'echo changed > "$FAKE_RELEASE/LICENSE"' \
  'echo extra >> "$FAKE_RELEASE/SHA256SUMS"'; do
  new_release
  FAKE_AFTER_UPLOAD=$change expect refuse "changed after upload ($change): not published" 'nothing was published' publish_macos twelve
  if published || ! no_publish_attempt; then fail "draft published after: $change"; fi
done
new_release
# shellcheck disable=SC2016 # expanded by the shim's shell
FAKE_AFTER_UPLOAD='echo 99 > "$FAKE_RELEASE.ids"' \
  expect refuse 'release replaced during the run: not published' 'changed during this run' publish_macos thirteen
if published || ! no_publish_attempt; then fail 'replaced release was published'; fi

new_release
# shellcheck disable=SC2016 # expanded by the shim's shell
FAKE_AFTER_EDIT='echo tampered > "$FAKE_RELEASE/git-ai-windows-arm64.exe"' \
  expect refuse 'published release that does not match is reported' "Published v$version does not match" publish_macos fourteen

new_release
grep -v ' git-ai-linux-arm64$' "$FAKE_RELEASE/SHA256SUMS" > "$tmp/sums" && mv "$tmp/sums" "$FAKE_RELEASE/SHA256SUMS"
expect refuse 'SHA256SUMS with 3 non-macOS lines is refused' "CI's binaries on v$version do not match" publish_macos fifteen

new_release
echo 'ci extra' > "$FAKE_RELEASE/git-ai-extra"
(cd "$FAKE_RELEASE" && shasum -a 256 git-ai-extra >> SHA256SUMS)
expect refuse 'SHA256SUMS with 5 non-macOS lines is refused' "CI's binaries on v$version do not match" publish_macos sixteen

# A macOS line that does not match its asset, or whose asset is missing, is replaced by a re-run.
new_release
echo 'macos old' > "$FAKE_RELEASE/git-ai-macos-arm64"
(cd "$FAKE_RELEASE" && shasum -a 256 git-ai-macos-arm64 >> SHA256SUMS)
echo 'macos changed' > "$FAKE_RELEASE/git-ai-macos-arm64"
expect pass 'macOS line not matching its asset: a re-run replaces both' '' publish_macos seventeen
new_release
(cd "$FAKE_RELEASE" && echo 'macos gone' > git-ai-macos-arm64 && shasum -a 256 git-ai-macos-arm64 >> SHA256SUMS && rm git-ai-macos-arm64)
expect pass 'macOS line with no asset: a re-run replaces it' '' publish_macos eighteen

# The dry run prints every --publish step in order and calls no command (the gh shim logs none).
step_line() { printf '%s\n' "$dry_run" | grep -n -m1 -F -- "$1" | cut -d: -f1; }
rm -f "$FAKE_LOG"
dry_run=$(bash "$script" --dry-run)
lines=()
for text in 'only release for v' "shasum -a 256 -c CI's four" 'cargo build --release' '(expect 5 lines)' \
  'gh release upload' 'verify the draft' '--draft=false' 'verify the published release'; do
  lines+=("$(step_line "$text")")
done
ordered=true
for ((i = 0; i < ${#lines[@]}; i++)); do
  [[ -n ${lines[i]} ]] || ordered=false
  if ((i > 0)) && [[ -n ${lines[i]} && -n ${lines[i - 1]} ]] && ((lines[i - 1] >= lines[i])); then ordered=false; fi
done
if $ordered && [[ $dry_run == *'nothing is built, signed, notarized, uploaded or published'* && ! -s $FAKE_LOG ]]; then
  echo 'ok   - dry run lists release check, verification, build, sums, upload, draft check, publish, published check'
else
  fail "dry run step order (lines ${lines[*]})"
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
