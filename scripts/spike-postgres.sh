#!/bin/zsh
# Starts the spike's-native postgres fixture (no container runtime needed).
# Usage: scripts/spike-postgres.sh start|stop
set -euo pipefail
PGBIN=/opt/homebrew/opt/postgresql@17/bin
PGDATA=/private/tmp/cb-spike-pgdata
PORT=54329

case "${1:-start}" in
  start)
    rm -rf "$PGDATA"
    "$PGBIN/initdb" -U postgres -A trust -D "$PGDATA" >/dev/null
    "$PGBIN/pg_ctl" -D "$PGDATA" -o "-p $PORT -c listen_addresses=127.0.0.1" \
      -l /private/tmp/cb-spike-pg.log start >/dev/null
    "$PGBIN/pg_isready" -h 127.0.0.1 -p "$PORT"
    ;;
  stop)
    "$PGBIN/pg_ctl" -D "$PGDATA" stop -m fast >/dev/null 2>&1 || true
    ;;
esac
