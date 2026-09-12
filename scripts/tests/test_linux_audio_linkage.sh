#!/usr/bin/env bash
# Exercise real ELF metadata; fixtures never initialize an audio backend.
set -euo pipefail
if [[ $(uname -s) != Linux ]]; then
  echo 'SKIP Linux audio linkage tests (requires Linux ELF tools)'
  exit 0
fi
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
check=$root/scripts/check-linux-audio-linkage.sh
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
printf 'int main(void) { return 0; }\n' > "$temporary/main.c"
printf 'void fixture(void) {}\n' > "$temporary/lib.c"
cc "$temporary/main.c" -o "$temporary/plain"
"$check" "$temporary/plain"
for library in libasound.so.2 libpulse.so.0 libpulse-simple.so.0; do
  cc -shared -fPIC "$temporary/lib.c" -Wl,-soname,"$library" -o "$temporary/$library"
  cc "$temporary/main.c" -Wl,--no-as-needed "$temporary/$library" -o "$temporary/linked"
  if "$check" "$temporary/linked" > "$temporary/output" 2>&1; then
    echo "failed to reject $library" >&2
    exit 1
  fi
  grep -q 'Audio libraries must not be startup dependencies.' "$temporary/output"
done
printf 'not an ELF binary\n' > "$temporary/invalid"
if "$check" "$temporary/invalid" > /dev/null 2>&1; then
  echo 'failed to reject invalid ELF input' >&2
  exit 1
fi
echo 'Linux audio linkage tests passed'
