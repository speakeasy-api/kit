#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
calculator=$root/scripts/next-release-version.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/kit-release-version.XXXXXX")
trap 'rm -rf "$temporary"' EXIT
case_number=0

new_repo() {
  case_number=$((case_number + 1))
  repo=$temporary/repo-$case_number
  mkdir "$repo"
  git -C "$repo" init -q
  git -C "$repo" config user.name test
  git -C "$repo" config user.email test@example.com
  printf 'base\n' > "$repo/changes"
  git -C "$repo" add changes
  git -C "$repo" commit -qm 'chore: release 1.2.3'
  git -C "$repo" tag v1.2.3
  base_branch=$(git -C "$repo" branch --show-current)
}

commit_change() {
  subject=$1
  body=${2-}
  printf '%s\n' "$subject" >> "$repo/changes"
  git -C "$repo" add changes
  if [ -n "$body" ]; then
    git -C "$repo" commit -qm "$subject" -m "$body"
  else
    git -C "$repo" commit -qm "$subject"
  fi
}

expect_version() {
  expected=$1
  actual=$(cd "$repo" && "$calculator" 1.2.3 v1.2.3 HEAD)
  if [ "$actual" != "$expected" ]; then
    echo "expected $expected, got $actual" >&2
    exit 1
  fi
}

expect_failure() {
  if (cd "$repo" && "$calculator" "$@") >/dev/null 2>&1; then
    echo "expected failure: $*" >&2
    exit 1
  fi
}

new_repo
commit_change 'docs: explain releases'
commit_change 'chore: update automation'
expect_version 1.2.4

new_repo
commit_change 'feat!: replace the protocol'
expect_version 1.3.0

new_repo
commit_change 'fix(api)!: reject the old protocol'
expect_version 1.3.0

new_repo
commit_change 'feat: add the protocol' 'BREAKING CHANGE: replaces the old protocol'
commit_change 'fix: follow up'
expect_version 1.3.0

new_repo
commit_change 'docs: mention BREAKING CHANGE: as prose'
expect_version 1.2.4

new_repo
expect_failure 1.2.3 v1.2.3 HEAD
commit_change 'fix: correct behavior'
expect_failure 1.02.3 v1.2.3 HEAD
expect_failure 1.2.3 HEAD v1.2.3

new_repo
commit_change 'update documentation'
expect_failure 1.2.3 v1.2.3 HEAD

new_repo
git -C "$repo" switch -qc topic
commit_change 'feat: add merged behavior'
git -C "$repo" switch -q "$base_branch"
git -C "$repo" merge --no-ff -qm 'Merge pull request #1 from example/topic' topic
expect_version 1.2.4

printf 'next release version tests passed\n'
