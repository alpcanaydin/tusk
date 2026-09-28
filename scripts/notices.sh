#!/bin/bash
# Print THIRD_PARTY_NOTICES.md: the licenses of everything Tusk.app ships.
# scripts/bundle.sh writes it into Contents/Resources; it is generated, so it
# never drifts from Cargo.lock or the bundled tool versions.
#   scripts/notices.sh > THIRD_PARTY_NOTICES.md
set -euo pipefail
cd "$(dirname "$0")/.."
brewv() { brew list --versions "$1" 2>/dev/null | awk '{print $2}' || true; }

cat <<EOF
# Third-party notices

Tusk is MIT-licensed (see LICENSE). The app bundle also ships the software
below, each under its own license.

## Programs and libraries in Contents/Resources

| Component | Version | License | Source |
| --- | --- | --- | --- |
| pg_dump, pg_restore, psql, libpq (PostgreSQL) | $(brewv libpq) | PostgreSQL License | https://www.postgresql.org/ftp/source/ |
| GNU Readline (libreadline, loaded by psql) | $(brewv readline) | GPL-3.0-or-later | https://ftp.gnu.org/gnu/readline/ |
| OpenSSL (libssl, libcrypto) | $(brewv openssl@3) | Apache-2.0 | https://github.com/openssl/openssl |
| MIT Kerberos (libkrb5, libgssapi_krb5, …) | $(brewv krb5) | MIT (Kerberos) | https://web.mit.edu/kerberos/dist/ |
| postgres-language-server | ${TUSK_PGLS_VERSION:-0.25.7} | MIT | https://github.com/supabase-community/postgres-language-server |
| sqls | ${TUSK_SQLS_VERSION:-v0.2.48} | MIT | https://github.com/sqls-server/sqls |
| Sparkle.framework | 2.10.0 | MIT | https://github.com/sparkle-project/Sparkle |

pg_dump, pg_restore and psql are separate programs that Tusk runs; they are
not linked into Tusk. psql dynamically loads GNU Readline, which is licensed
under the GNU General Public License v3 (https://www.gnu.org/licenses/gpl-3.0.txt).
The complete corresponding source of Readline is available at the link
above; on request we will provide it as well.

## Rust crates compiled into Tusk

Generated from Cargo.lock (\`cargo metadata\`). Each crate's license text
ships with its source on crates.io.

| Crate | Version | License |
| --- | --- | --- |
EOF
cargo metadata --format-version 1 --locked 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
for p in sorted(d["packages"], key=lambda p: (p["name"], p["version"])):
    if p["name"] == "tusk":
        continue
    print("| %s | %s | %s |" % (p["name"], p["version"], p.get("license") or "see crate"))
'
