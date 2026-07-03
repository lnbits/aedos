#!/usr/bin/env sh
set -eu

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
backup_dir="${BACKUP_DIR:-./backups/postgres}"
compose="${COMPOSE:-docker compose}"

mkdir -p "$backup_dir"

$compose exec -T postgres pg_dump \
  -U oracle \
  -d oracle \
  --format=custom \
  --no-owner \
  --no-acl \
  > "$backup_dir/aedos-postgres-$timestamp.dump"

echo "$backup_dir/aedos-postgres-$timestamp.dump"
