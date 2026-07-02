<h1 style="font-size:5em !important"> <img width="60px" src="https://github.com/user-attachments/assets/259e50cb-fd73-4218-8a29-360293828186"> Aedos</h1>

`Aedos` is an AI-powered moderation oracle for Nostr. It checks images and events, caches the result, and publishes signed moderation labels that clients and relays can choose to trust.

Nostr gives users and relays a lot of freedom, but that also means every client or relay is left to solve abuse, spam, NSFW media, graphic content, and illegal material on its own. Aedos turns moderation into portable infrastructure: one service can review media once, store the verdict by event ID and image hash, and make that signal reusable across the network.

## What Is Included

- Rust oracle service with HTTP and WebSocket APIs.
- Postgres schema for events, images, verdicts, reports, and published labels.
- Redis-backed analysis queue.
- Python worker with image download, SHA-256, perceptual hash, and a swappable moderation model interface.
- Swappable moderation providers. The default deterministic provider is for development; `MODERATION_PROVIDER=openai` enables OpenAI image moderation.
- NIP-32 label draft generation using kind `1985`, `L` namespace tags, matching `l` label marks, and target tags.
- Realtime verdict event draft generation with configurable `ORACLE_VERDICT_KIND` defaulting to `31494`.
- Emergency escalation records for `csam-suspected` verdicts. These store audit metadata such as event ID, URL, hashes, confidence, source, and status; they do not store image bytes.
- SSRF guardrails for localhost, loopback, private, and link-local URL targets.
- Docker Compose for Postgres, Redis, Rust oracle, Python worker, and SvelteKit admin dashboard.

## Quick Start

1. Copy the example environment file:

```bash
cp .env.example .env
```

2. Start the full stack:

```bash
docker compose up --build
```

This starts:

- Postgres
- Redis
- Rust oracle API
- Python moderation worker
- SvelteKit admin dashboard

3. Open the dashboard:

```text
http://localhost:3000
```

On first load, create the first admin username and password. After setup, the dashboard uses an HttpOnly session cookie for login.

4. Check the oracle API:

```bash
curl http://localhost:8080/health
```

5. Submit a test image event:

```bash
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -d '{"event_id":"example","image_urls":["https://example.com/image.png"]}'
```

The first response may be `unknown` while the worker downloads, hashes, and reviews the image. Later calls for the same event ID return the cached event verdict. New events with an already-seen image SHA-256 reuse the cached image verdict and do not call the AI provider again.

Useful local URLs:

- Dashboard: `http://localhost:3000`
- Oracle API: `http://localhost:8080`
- Health: `http://localhost:8080/health`
- Metrics: `http://localhost:8080/metrics`

To run in the background:

```bash
docker compose up --build -d
```

To stop:

```bash
docker compose down
```

By default, Aedos uses the deterministic development provider, which marks valid images as safe and does not call any external AI service.

## OpenAI Image Moderation

OpenAI image moderation is the easiest production reviewer to enable first. The worker sends OpenAI only new image hashes that are not already cached by Aedos.

1. Create an OpenAI API key.

2. Either edit `.env` before startup:

```env
MODERATION_PROVIDER=openai
OPENAI_API_KEY=sk-...
OPENAI_MODERATION_MODEL=omni-moderation-latest
```

Or start Aedos with the deterministic provider, open the dashboard, and update these settings there. Dashboard settings are stored in Postgres and are hot-applied by the worker on its next queue loop. Aedos will reject `MODERATION_PROVIDER=openai` unless an `OPENAI_API_KEY` is also set.

3. Start or restart Aedos:

```bash
docker compose up --build
```

4. Check health:

```bash
curl http://localhost:8080/health
```

5. Submit an image check:

```bash
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -d '{"event_id":"example","image_urls":["https://example.com/image.png"]}'
```

The first response may be `unknown` while the worker downloads, hashes, and reviews the image. Later calls for the same event ID return the cached event verdict. New events with an already-seen image SHA-256 reuse the cached image verdict and do not call OpenAI again.

## Queue Reliability

Analysis jobs are stored in a Redis Stream with a worker consumer group. Workers acknowledge a job only after it has been processed successfully. Failed jobs are retried with exponential backoff and then moved to a dead-letter stream after the retry limit is reached. Active and dead-letter streams are capped with Redis `MAXLEN` so a busy relay can run continuously without unbounded queue growth.

Queue keys:

- `oracle:analysis`: active analysis stream
- `oracle:analysis:retry`: delayed retry set
- `oracle:analysis:dead`: dead-letter stream

This gives Aedos safe restart behavior for normal worker crashes and provider failures without dropping jobs silently.

## Dashboard

The SvelteKit dashboard lives in `apps/dashboard` and is included in Docker Compose.

It provides:

- Overview stats for processed images, daily processing, queue depth, retries, and dead letters.
- A searchable, paginated image table with event ID search.
- Operator review controls for changing an image verdict.
- A settings page for allowlisted operational values such as moderation limits, queue retention, rate limits, Nostr relay config, and provider settings.

Settings are stored in Postgres and secret values are masked when read back. Worker/provider settings are hot-applied by the Python worker on its next queue loop; public API rate limits and queue retention are also read from the stored settings. The dashboard is intentionally not a raw remote `.env` file editor.

Hot-applied settings include:

- `MODERATION_PROVIDER`
- `OPENAI_API_KEY`
- `OPENAI_MODERATION_MODEL`
- `MAX_IMAGE_BYTES`
- `IMAGE_FETCH_TIMEOUT_SECONDS`
- `QUEUE_STREAM_MAXLEN`
- `QUEUE_DEAD_LETTER_MAXLEN`
- `RATE_LIMIT_CHECKS_PER_MINUTE`

Some boot-level settings still require restarting the relevant service after editing `.env`, such as database URLs, Redis URLs, bind ports, and Compose port mappings.

## API

- `POST /v1/check`
- `POST /v1/check_batch`
- `POST /v1/submit`
- `GET /v1/event/:event_id`
- `GET /v1/image/:sha256`
- `GET /v1/ws`
- `GET /health`
- `GET /metrics`

WebSocket messages:

```json
{"type":"check","event_id":"...","image_urls":["https://example.com/a.png"]}
```

```json
{"type":"check_batch","events":[{"event_id":"...","image_urls":[]}]}
```

## Nostr Compliance Notes

The implementation references the local `nips/` folder:

- NIP-01 event/tag conventions.
- NIP-32 labeling: kind `1985`, `L` namespace tag, `l` labels with matching namespace mark, and `e`/`p`/`r`/`x` targets.
- NIP-56 report schema is represented in the database for report ingestion.

The production relay publisher is isolated in `crates/oracle/src/nostr.rs`; current tests validate the NIP-shaped event drafts without needing external relays or private keys.

## Emergency Moderation Notes

`csam-suspected` is treated as a high-severity block label, not as an ordinary moderation category. When a worker model emits that label, the worker stores the normal verdict and adds an `emergency_escalations` row with metadata for an operator-controlled process.

This project intentionally avoids storing image bytes for escalations. Operators should define access controls, retention, reporting obligations, and review procedures for their jurisdiction before enabling any production model that can emit emergency labels.

## Test

Rust:

```bash
cargo test
```

Python:

```bash
cd workers/python
uv run pytest
```

## Environment

See `.env.example` for all settings. Important values:

- `DATABASE_URL`
- `REDIS_URL`
- `NOSTR_PRIVATE_KEY`
- `NOSTR_RELAYS`
- `LABEL_NAMESPACE`
- `ORACLE_VERDICT_KIND`
- `MAX_IMAGE_BYTES`
- `IMAGE_FETCH_TIMEOUT_SECONDS`
- `QUEUE_CONSUMER_GROUP`
- `QUEUE_CONSUMER_NAME`
- `QUEUE_STREAM_MAXLEN`
- `QUEUE_DEAD_LETTER_MAXLEN`
- `RATE_LIMIT_CHECKS_PER_MINUTE`
- `MODERATION_PROVIDER`
- `OPENAI_API_KEY`
- `OPENAI_MODERATION_MODEL`
