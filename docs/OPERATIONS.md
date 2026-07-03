# Aedos Operations

This runbook defines the minimum production-pilot operations for Aedos: backups, retention, migrations, monitoring, and alerting.

## Backups

Back up Postgres. Redis is treated as queue/runtime state; losing Redis can drop in-flight jobs, but completed verdicts and dashboard state live in Postgres.

Recommended schedule:

- Postgres full logical backup: daily.
- Retention of backup files: at least 14 daily backups and 4 weekly backups.
- Restore test: monthly, and before any production rollout that changes migrations.

Create a backup:

```bash
scripts/backup-postgres.sh
```

Use a custom destination:

```bash
BACKUP_DIR=/srv/aedos/backups/postgres scripts/backup-postgres.sh
```

Restore into the running Compose Postgres service:

```bash
scripts/restore-postgres.sh /srv/aedos/backups/postgres/aedos-postgres-YYYYMMDDTHHMMSSZ.dump
```

Production notes:

- Store backups outside the repo and outside the Docker volume.
- Encrypt backups at rest if they leave the host.
- Keep backups access-controlled. They may contain Nostr event content, URLs, settings, verdict history, and admin account data.
- Do not rely on Docker volume snapshots alone unless you have tested restore from them.

Cron example:

```cron
15 2 * * * cd /opt/aedos && BACKUP_DIR=/srv/aedos/backups/postgres scripts/backup-postgres.sh
```

## Retention

Aedos intentionally does not store image or video bytes, but the database can still grow through events, media metadata, verdicts, job state, sessions, settings, and provider response summaries.

Recommended starting retention:

- Verdicts: 180 days.
- Events and media metadata without remaining verdict references: 180 days.
- Expired admin sessions: 30 days.
- Completed/failed analysis job rows: 180 days.
- Emergency escalation rows: keep pending rows until operator process resolves them; only resolved/non-pending rows are eligible for cleanup.

Run retention cleanup:

```bash
docker compose exec -T postgres psql -U oracle -d oracle \
  -v verdict_days=180 \
  -v event_days=180 \
  -v session_days=30 \
  -f /dev/stdin < scripts/retention.sql
```

Cron example:

```cron
45 3 * * 0 cd /opt/aedos && docker compose exec -T postgres psql -U oracle -d oracle -v verdict_days=180 -v event_days=180 -v session_days=30 -f /dev/stdin < scripts/retention.sql
```

Before reducing retention windows:

- Confirm your local moderation, legal, and audit requirements.
- Confirm backups are completing and restorable.
- Keep `csam-suspected` operational process separate from this generic cleanup.

## Migration Strategy

Current Compose behavior mounts `migrations/` into Postgres init, so migrations run automatically only when the Postgres volume is created for the first time. Some additive schema maintenance is also applied at runtime by the API/dashboard for newer optional tables.

Production rollout process:

1. Read the diff for `migrations/`, `crates/oracle/src/*db*`, `crates/oracle/src/admin.rs`, and `crates/oracle/src/api.rs`.
2. Take a fresh Postgres backup.
3. Test restore and startup on a staging copy if the schema changed.
4. Stop traffic or put the relay/client integration in fail-closed/hold mode.
5. Pull/build the new version.
6. Apply any new explicit migration SQL to Postgres if the release includes one.
7. Start services.
8. Check `/health`, `/metrics`, dashboard overview, Redis queue depth, and a sample `/v1/check`.
9. Keep the previous image/commit and backup available for rollback.

Rules for future schema changes:

- Do not edit old migration files after release. Add a new numbered migration file.
- Make migrations idempotent when practical: `create table if not exists`, `alter table ... add column if not exists`.
- Avoid destructive migrations in the same deploy as application code unless there is a tested rollback path.
- Run `cargo test`, Python worker tests, dashboard checks, and `docker compose config` before release.

## Monitoring

Minimum checks:

- `GET /health`: API process is alive.
- `GET /metrics`: scrapeable Prometheus metrics.
- Docker health checks for Postgres and Redis.
- Dashboard overview: incoming, processing, retry, dead-letter, safe/warn/block/error counts.
- Worker logs: provider failures, image/video fetch failures, retry/dead-letter movement.

Prometheus scrape example:

```yaml
scrape_configs:
  - job_name: aedos-oracle
    metrics_path: /metrics
    static_configs:
      - targets: ["127.0.0.1:8080"]
```

If `API_KEYS` is configured, put Prometheus behind the same trusted network path or configure it to send an API key. Prefer a reverse proxy that injects the header rather than putting keys in URLs.

Example alert rules are in:

```text
monitoring/prometheus-alerts.yml
```

Important metrics currently exposed:

- `cache_hits`
- `cache_misses`
- `queued_jobs`
- `analysed_images`
- `published_labels`
- `published_verdict_events`
- `connected_clients`
- `connected_relays`

Useful manual checks:

```bash
docker compose ps
docker compose logs --tail=200 oracle
docker compose logs --tail=200 worker
docker compose exec redis redis-cli XLEN oracle:analysis
docker compose exec redis redis-cli ZCARD oracle:analysis:retry
docker compose exec redis redis-cli XLEN oracle:analysis:dead
```

## Alerting

Minimum alerts:

- API down: `/metrics` scrape fails for 2 minutes.
- Queue not draining: `queued_jobs` increases but `analysed_images` does not increase.
- Dead-letter growth: Redis `oracle:analysis:dead` grows over the last check interval.
- Provider failures: worker logs show repeated OpenAI/rate-limit/fetch errors.
- Label publishing stalled: event verdicts exist but `published_labels` is not increasing when label publishing is enabled.
- Disk usage: host volume/backups disk over 80%.
- Backup missing: no successful backup file in the expected backup path within 26 hours.

When an alert fires:

1. Check `docker compose ps`.
2. Check `oracle` and `worker` logs.
3. Check Redis queue lengths.
4. Check OpenAI provider/key/rate-limit settings if using OpenAI.
5. Decide whether relays/clients should fail closed, hold unknowns, or use cached labels only.

## Recovery

If Postgres is lost:

1. Stop Aedos services.
2. Recreate the Postgres volume.
3. Start Postgres.
4. Restore the latest backup.
5. Start Redis, oracle, worker, and dashboard.
6. Validate `/health`, dashboard login, and a sample check.

If Redis is lost:

1. Restart Redis.
2. Restart worker.
3. Re-submit or recheck any events that were queued but not completed.
4. Use the dashboard failed/retry state and relay/client unknowns to identify what needs rechecking.

If the worker is failing:

1. Set relays/clients to hold or reject unknown media depending on policy.
2. Check provider configuration and rate limits.
3. Check fetch errors and `MAX_IMAGE_BYTES`/`MAX_VIDEO_BYTES`.
4. Re-run failed items from the dashboard after fixing the cause.
