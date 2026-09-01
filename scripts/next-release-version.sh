#!/bin/sh
set -eu

usage() {
  echo "usage: $0 CURRENT_VERSION FROM_REF TO_REF" >&2
  exit 2
}

die() {
  echo "next-release-version: $*" >&2
  exit 1
}

[ "$#" -eq 3 ] || usage

current_version=$1
from_ref=$2
to_ref=$3

case $current_version in
  0 | *[!0-9.]* | .* | *. | *..*) die "invalid current version: $current_version" ;;
esac

old_ifs=$IFS
IFS=.
set -- $current_version
IFS=$old_ifs
[ "$#" -eq 3 ] || die "invalid current version: $current_version"
major=$1
minor=$2
patch=$3

for component in "$major" "$minor" "$patch"; do
  case $component in
    0 | [1-9] | [1-9][0-9]*) ;;
    *) die "invalid current version: $current_version" ;;
  esac
done

git rev-parse --verify "$from_ref^{commit}" >/dev/null 2>&1 ||
  die "unknown release ref: $from_ref"
git rev-parse --verify "$to_ref^{commit}" >/dev/null 2>&1 ||
  die "unknown target ref: $to_ref"
git merge-base --is-ancestor "$from_ref" "$to_ref" ||
  die "release ref $from_ref is not an ancestor of $to_ref"

commit_count=$(git rev-list --count "$from_ref..$to_ref")
[ "$commit_count" -gt 0 ] || die "no commits to release"

invalid_commits=$(
  git log --format='%h %s' "$from_ref..$to_ref" |
    grep -Ev '^[0-9a-f]+ [a-z][a-z0-9-]*(\([^()]+\))?!?: .+' || true
)
if [ -n "$invalid_commits" ]; then
  die "non-Conventional Commit subjects:
$invalid_commits"
fi

bump=patch
if git log --format='%s' "$from_ref..$to_ref" |
    grep -Eq '^[a-z][a-z0-9-]*(\([^()]+\))?!: .+'; then
  bump=minor
elif git log --format='%b' "$from_ref..$to_ref" |
    grep -Eq '^BREAKING CHANGE:($|[[:space:]])'; then
  bump=minor
fi

if [ "$bump" = minor ]; then
  printf '%s.%s.0\n' "$major" "$((minor + 1))"
else
  printf '%s.%s.%s\n' "$major" "$minor" "$((patch + 1))"
fi
