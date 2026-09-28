#!/bin/bash
# Put the SQL language server / linter (postgres-language-server, a single
# native binary) into <dest>/postgres-language-server so the editor's
# completions and diagnostics work without npm.
#   scripts/bundle-pgls.sh <dest>
# Source: $TUSK_PGLS_SRC, else the npm platform package (downloaded).
set -euo pipefail
dest="$1"
version="${TUSK_PGLS_VERSION:-0.25.7}"
mkdir -p "$dest"
if [ -n "${TUSK_PGLS_SRC:-}" ]; then
  cp -f "$TUSK_PGLS_SRC" "$dest/postgres-language-server"
else
  arch=$([ "$(uname -m)" = arm64 ] && echo aarch64 || echo x86_64)
  pkg="cli-$arch-apple-darwin"
  tmp=$(mktemp -d)
  curl -fsSL "https://registry.npmjs.org/@postgres-language-server/$pkg/-/$pkg-$version.tgz" \
    | tar -xz -C "$tmp"
  cp -f "$tmp/package/postgres-language-server" "$dest/postgres-language-server"
  rm -rf "$tmp"
fi
chmod +x "$dest/postgres-language-server"
codesign --force --sign - "$dest/postgres-language-server" 2>/dev/null
echo "bundled postgres-language-server $version"
