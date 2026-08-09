#!/bin/sh
# Re-sign a freshly built kit binary with a stable signing identity.
#
# cargo's default ad-hoc linker signature changes every build, so macOS
# keychain ACLs treat each rebuild as a new program and re-prompt (or wedge a
# headless daemon inside SecKeychainFindGenericPassword). Signing with a real
# certificate and a fixed identifier keeps the designated requirement stable:
# approve keychain access once with "Always Allow" and rebuilds stay silent.
#
# Usage: scripts/dev_sign.sh [path-to-binary]
#   KIT_SIGN_IDENTITY overrides the certificate (see `security find-identity
#   -v -p codesigning` for available identities).
set -eu
IDENTITY="${KIT_SIGN_IDENTITY:-Apple Development: DANIEL EMOD KOVACS (JH8AA74PW4)}"
exec codesign --force -s "$IDENTITY" -i com.speakeasy.kit "${1:-target/debug/kit}"
