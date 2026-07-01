<h1 style="font-size:4em"> <img width="60px" src="https://github.com/user-attachments/assets/259e50cb-fd73-4218-8a29-360293828186"> Aedos</h1>

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
- Docker Compose for Postgres, Redis, Rust oracle, and Python worker.

## Quick Start

```bash
cp .env.example .env
docker compose up --build
```

The oracle listens on `http://localhost:8080` by default.

```bash
curl http://localhost:8080/health
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -d '{"event_id":"example","image_urls":["https://example.com/image.png"]}'
```

By default, Aedos uses the deterministic development provider, which marks valid images as safe and does not call any external AI service.

## OpenAI Image Moderation

OpenAI image moderation is the easiest production reviewer to enable first. The worker sends OpenAI only new image hashes that are not already cached by Aedos.

1. Create an OpenAI API key.

2. Edit `.env`:

```env
MODERATION_PROVIDER=openai
OPENAI_API_KEY=sk-...
OPENAI_MODERATION_MODEL=omni-moderation-latest
```

3. Start Aedos:

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
- `MODERATION_PROVIDER`
- `OPENAI_API_KEY`
- `OPENAI_MODERATION_MODEL`
