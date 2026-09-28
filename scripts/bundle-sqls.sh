#!/bin/bash
# Put the multi-database SQL language server (sqls: MySQL / MariaDB,
# PostgreSQL-protocol engines, SQLite, SQL Server, Vertica, ClickHouse)
# into <dest>/sqls as a universal binary.
#   scripts/bundle-sqls.sh <dest>
# Source: $TUSK_SQLS_SRC (a built binary), else built from the tagged
# release with Go (the published macOS build is x86_64 only and cgo-less).
set -euo pipefail
dest="$1"
version="${TUSK_SQLS_VERSION:-v0.2.48}"
mkdir -p "$dest"
if [ -n "${TUSK_SQLS_SRC:-}" ]; then
  cp -f "$TUSK_SQLS_SRC" "$dest/sqls"
else
  command -v go >/dev/null || { echo "bundle-sqls: needs Go (brew install go)" >&2; exit 1; }
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  git clone -q --depth 1 --branch "$version" https://github.com/sqls-server/sqls "$tmp/src"
  # Its SQL Server catalog queries bind `@p1`, which only the `sqlserver`
  # driver name accepts (`mssql` is the legacy `?` flavour).
  sed -i '' 's/sql.Open("mssql", dsn)/sql.Open("sqlserver", dsn)/' "$tmp/src/internal/database/mssql.go"
  # Its Vertica catalog errors on foreign keys, which drops the whole
  # completion cache: report none instead.
  sed -i '' -e 's/return nil, fmt.Errorf("describe foreign keys is not supported")/return nil, nil/' \
    -e '/^	"fmt"$/d' "$tmp/src/internal/database/vertica.go"
  for arch in arm64 amd64; do
    cc_arch=$([ "$arch" = amd64 ] && echo x86_64 || echo arm64)
    (cd "$tmp/src" && CGO_ENABLED=1 GOARCH=$arch CC="clang -arch $cc_arch" GOFLAGS=-mod=mod \
      go build -trimpath -ldflags "-s -w" -o "$tmp/sqls-$arch" .)
  done
  lipo -create -output "$dest/sqls" "$tmp/sqls-arm64" "$tmp/sqls-amd64"
fi
chmod +x "$dest/sqls"
codesign --force --sign - "$dest/sqls" 2>/dev/null
echo "bundled sqls $version"
