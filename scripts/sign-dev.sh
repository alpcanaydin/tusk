#!/bin/bash
# Sign a dev build with a stable identity so the macOS Keychain's "Always
# Allow" sticks across rebuilds. Ad-hoc signatures change every build, and
# Keychain ACLs trust a binary's code requirement, so an unsigned dev build
# re-prompts for the saved password on every launch.
#
# Identity: $TUSK_SIGN_IDENTITY, else the first "Apple Development" identity.
# No identity → leave the binary as is (the prompt just comes back).
set -euo pipefail
bin="$1"
id="${TUSK_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | sed -n 's/.*"\(Apple Development: [^"]*@reyz\.ai[^"]*\)".*/\1/p' | head -1)}"
if [ -n "$id" ]; then
  codesign --force --sign "$id" --identifier ai.reyz.tusk "$bin" 2>/dev/null || \
    echo "sign-dev: codesign failed, running unsigned" >&2
fi
