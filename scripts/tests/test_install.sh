#!/usr/bin/env bash
# Exercise the real installer with local release fixtures, never a network request.
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
temporary=$(mktemp -d)
pid=""
trap 'if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi; rm -rf "$temporary"' EXIT
mkdir -p "$temporary/tools" "$temporary/release" "$temporary/stage" "$temporary/install dir"
cat > "$temporary/tools/uname" <<'SH'
#!/bin/sh
case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 1 ;; esac
SH
cat > "$temporary/tools/curl" <<'SH'
#!/bin/sh
set -eu
# install.sh's archive/checksum requests always use this exact argument shape.
[ "$1" = -fsSL ] && [ "$2" = -o ] && [ "$#" = 4 ]
cp "$FIXTURE_RELEASE/${4##*/}" "$3"
SH
chmod +x "$temporary/tools/"*
printf '#!/bin/sh\necho updated\n' > "$temporary/stage/kit"
asset=kit-vtest-x86_64-unknown-linux-gnu.tar.gz
tar -czf "$temporary/release/$asset" -C "$temporary/stage" kit
printf '#include <unistd.h>\nint main(void) { for (;;) sleep(1); }\n' > "$temporary/sleeper.c"
cc "$temporary/sleeper.c" -o "$temporary/sleeper"
cp "$temporary/sleeper" "$temporary/install dir/kit"
"$temporary/install dir/kit" 60 &
pid=$!
export FIXTURE_RELEASE="$temporary/release"
export PATH="$temporary/tools:$PATH"
export KIT_VERSION=vtest
export KIT_INSTALL_DIR="$temporary/install dir"
printf 'bad  %s\n' "$asset" > "$temporary/release/SHA256SUMS"
if sh "$root/install.sh" > "$temporary/output" 2>&1; then
  echo 'installer accepted an incorrect checksum' >&2
  exit 1
fi
grep -q 'checksum mismatch' "$temporary/output"
cmp "$temporary/sleeper" "$KIT_INSTALL_DIR/kit"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$FIXTURE_RELEASE" && sha256sum "$asset") > "$FIXTURE_RELEASE/SHA256SUMS"
else
  (cd "$FIXTURE_RELEASE" && shasum -a 256 "$asset") > "$FIXTURE_RELEASE/SHA256SUMS"
fi
sh "$root/install.sh" > "$temporary/output" 2>&1
[[ $("$KIT_INSTALL_DIR/kit") = updated ]]
kill -0 "$pid" # the old executable remains alive after replacement (including Linux)
[[ $(find "$KIT_INSTALL_DIR" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ') = 1 ]]
echo 'Installer checksum and running-executable replacement tests passed'
