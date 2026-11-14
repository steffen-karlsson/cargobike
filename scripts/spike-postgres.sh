#!/bin/zsh
# Starts the spike's test postgres inside podman (one container, the
# same URL contract as CI). Usage: scripts/spike-postgres.sh start|stop|status
#
# The URL contract (AGENTS.md "Environment notes"):
#   CARGOBIKE_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:54329/postgres
#
# `start` removes any container left from a previous run/boot FIRST —
# stale instances must never accumulate on a machine (that is a crash
# vector, not a fixture) — then boots one postgres:17-alpine and waits
# for readiness.
set -euo pipefail
CONTAINER=cb-spike-postgres
PORT=54329
IMAGE=docker.io/library/postgres:17-alpine
URL=postgres://postgres:postgres@127.0.0.1:${PORT}/postgres

case "${1:-start}" in
  start)
    # Exactly one instance per machine: a leftover container from a
    # previous run or an unclean shutdown is removed before boot.
    if podman container exists "$CONTAINER"; then
      podman rm -f "$CONTAINER" >/dev/null
    fi
    podman run -d --name "$CONTAINER" \
      -p 127.0.0.1:${PORT}:5432 \
      -e POSTGRES_PASSWORD=postgres \
      "$IMAGE" >/dev/null
    ready=0
    for _ in {1..60}; do
      if podman exec "$CONTAINER" pg_isready -U postgres -q >/dev/null 2>&1; then
        ready=1
        break
      fi
      sleep 1
    done
    if [ "$ready" != 1 ]; then
      echo "the spike postgres never became ready; logs:" >&2
      podman logs "$CONTAINER" >&2
      podman rm -f "$CONTAINER" >/dev/null 2>&1 || true
      exit 1
    fi
    # max_connections=300: the workspace's test binaries boot several
    # servers/instances against this fixture in parallel; 100 would
    # starve runs that are all individually honest.
    podman exec "$CONTAINER" psql -U postgres -q \
      -c 'ALTER SYSTEM SET max_connections = 300' >/dev/null
    podman restart "$CONTAINER" >/dev/null
    for _ in {1..60}; do
      if podman exec "$CONTAINER" pg_isready -U postgres -q >/dev/null 2>&1; then
        echo "CARGOBIKE_TEST_DATABASE_URL=${URL}"
        exit 0
      fi
      sleep 1
    done
    echo "the spike postgres never became ready after the max_connections restart" >&2
    exit 1
    ;;
  stop)
    podman rm -f "$CONTAINER" >/dev/null 2>&1 || true
    echo "stopped (the container is gone)"
    ;;
  status)
    if podman container exists "$CONTAINER"; then
      podman ps --filter name="^${CONTAINER}$" --format '{{.Status}}'
    else
      echo "absent"
    fi
    ;;
  *)
    echo "usage: $0 start|stop|status" >&2
    exit 2
    ;;
esac
