#!/usr/bin/env sh
set -eu

if [ "${1:-}" = "" ]; then
  echo "usage: scripts/restore-postgres.sh backups/postgres/aedos-postgres-YYYYMMDDTHHMMSSZ.dump" >&2
  exit 2
fi

backup_file="$1"
compose="${COMPOSE:-docker compose}"

$compose exec -T postgres pg_restore \
  -U oracle \
  -d oracle \
  --clean \
  --if-exists \
  --no-owner \
  --no-acl \
  < "$backup_file"
