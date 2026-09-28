#!/bin/bash
# CI: start PostgreSQL 17 on :55432 with the role, password and seed that
# docker-compose.yml sets up (tusk / tusk / tusk_dev), for the integration tests.
set -euo pipefail
brew install --quiet postgresql@17
bin="$(brew --prefix postgresql@17)/bin"
data="$RUNNER_TEMP/pgdata"
pw="$RUNNER_TEMP/pgpass"
printf 'tusk' > "$pw"
# Password auth, so the wrong-password test gets a real error.
"$bin/initdb" -D "$data" -U tusk --pwfile="$pw" --auth=scram-sha-256 >/dev/null
"$bin/pg_ctl" -D "$data" -o "-p 55432" -l "$RUNNER_TEMP/pg.log" -w start
export PGPASSWORD=tusk
"$bin/createdb" -h 127.0.0.1 -p 55432 -U tusk tusk_dev
for f in seed/*.sql; do
  "$bin/psql" -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -p 55432 -U tusk -d tusk_dev -f "$f"
done
