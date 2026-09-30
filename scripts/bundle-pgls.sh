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
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) pkg=cli-aarch64-apple-darwin ;;
    Darwin:x86_64) pkg=cli-x86_64-apple-darwin ;;
    Linux:aarch64) pkg=cli-aarch64-linux-gnu ;;
    Linux:x86_64) pkg=cli-x86_64-linux-gnu ;;
    *) echo "bundle-pgls: unsupported platform" >&2; exit 1 ;;
  esac
  tmp=$(mktemp -d)
  curl -fsSL "https://registry.npmjs.org/@postgres-language-server/$pkg/-/$pkg-$version.tgz" \
    | tar -xz -C "$tmp"
  cp -f "$tmp/package/postgres-language-server" "$dest/postgres-language-server"
  rm -rf "$tmp"
fi
chmod +x "$dest/postgres-language-server"
if [ "$(uname -s)" = Darwin ]; then
  codesign --force --sign - "$dest/postgres-language-server" 2>/dev/null
fi
echo "bundled postgres-language-server $version"
