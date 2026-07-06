<h1 style="font-size:5em !important"><img width="60px" src="images/logo.png"> Aedos</h1>

`Aedos` is an AI-powered moderation oracle for Nostr. It reviews notes, images, and videos, caches verdicts, and produces Nostr-native moderation labels that clients and relays can choose to trust.

Nostr gives users, clients, and relays freedom, but it also means every app is left to solve abuse, spam, NSFW media, graphic content, and illegal material on its own. Aedos turns that work into reusable infrastructure: review content once, store a lean verdict by event ID and media hash, and make the result available through HTTP, WebSockets, and Nostr label events.

## What Aedos Does

- Checks Nostr events, text tags, image URLs, and direct video URLs.
- Caches by event ID first, then by image/video SHA-256, so known media is not sent to the AI provider again.
- Uses a swappable moderation provider interface.
- Ships with a deterministic local provider for development.
- Supports OpenAI image moderation with `MODERATION_PROVIDER=openai`.
- Samples video frames with `ffmpeg` and reviews those frames through the configured image moderation provider.
- Detects high-risk text tags such as `#csam`, `#pedo`, and `#loli`, plus NSFW tags such as `#nsfw`, `#porn`, and `#nudity`.
- Stores verified author/pubkey links from signed Nostr events so Aedos can expose NSFW and CSAM-suspected author lists without trusting caller-supplied authors.
- Stores compact provider response details for audit/debugging, not full media bytes.
- Provides a SvelteKit admin dashboard with login, stats, media review, recheck actions, settings, theme toggle, relay status, and job error visibility.
- Generates NIP-32 label drafts using kind `1985`.
- Publishes stored event verdicts as NIP-32 labels when `NOSTR_PRIVATE_KEY`, `NOSTR_RELAYS`, and `ENABLE_LABEL_PUBLISHER=true` are configured.
- Serves a public moderation policy page at `/moderation`; by default the NIP-32 label namespace is derived from `PUBLIC_BASE_URL + /moderation`.

## Current Limits

- Relay publishing is implemented as a background publisher for stored event verdicts. It still needs real relay soak testing before being treated as relay-scale infrastructure.
- Video review checks sampled visual frames only. It does not inspect audio, subtitles, or HLS playlists.
- The text review layer is rule-based and focused on explicit Nostr tags/hashtags.
- Text marker lists are configurable from the dashboard. They only match hashtags and Nostr `t` tags, not arbitrary plain-text mentions.
- OpenAI OAuth is not used. Aedos currently expects an API key.
- `csam-suspected` records are moderation signals for operator/legal process. Aedos does not store image or video bytes for these escalations.

## Quick Start

Start the stack:

```bash
docker compose up --build
```

No `.env` file is required for first launch. By default, Compose uses local-only published ports and a development Postgres password inside the Docker network.

To customize before launch, copy the example environment file:

```bash
cp .env.example .env
```

Published ports can be changed in `.env`:

```env
DASHBOARD_PORT=3001
ORACLE_PORT=8081
PUBLIC_ORACLE_BASE_URL=http://localhost:8081
POSTGRES_PORT=5433
REDIS_PORT=6380
```

Or for a one-off run:

```bash
DASHBOARD_PORT=3001 ORACLE_PORT=8081 docker compose up --build
```

Most Aedos settings can also be changed later from the dashboard UI after you create the first admin account.

This starts:

- Postgres
- Redis
- Rust oracle API
- Python moderation worker
- SvelteKit public page and admin dashboard

Open Aedos:

```text
http://localhost:3000
```

Operator login and first-install setup live at:

```text
http://localhost:3000/login
```

The admin control surface lives at:

```text
http://localhost:3000/admin
```

The admin dashboard uses a browser WebSocket for live media updates. If you change `ORACLE_PORT` or run Aedos behind a domain/reverse proxy, set `PUBLIC_ORACLE_BASE_URL` to the browser-reachable oracle URL, for example `https://aedos.example` or `http://server-ip:8081`.

On first login, create the first admin account. The dashboard stores the password with Argon2 and uses an HttpOnly, SameSite session cookie.

`API_KEYS` is an Aedos access-control setting, not an OpenAI key. Create your own long random value and save it in the dashboard Settings page or in `.env` as `API_KEYS=...`. If `API_KEYS` is blank, public `/v1/*` checks do not need the `x-api-key` header.

If `API_KEYS` is set, public `/v1/*` and `/metrics` requests must include one of the configured keys:

```bash
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -H 'x-api-key: your-key' \
  -d '{"event_id":"signed-event-id-to-fetch-from-relays"}'
```

HTTP `/v1/*` requests can also go through the dashboard server, which is useful when you expose one public app port:

```bash
curl -X POST http://localhost:3000/v1/check \
  -H 'content-type: application/json' \
  -H 'x-api-key: your-key' \
  -d '{"event_id":"signed-event-id-to-fetch-from-relays"}'
```

API keys are accepted as `x-api-key`, `Authorization: Bearer ...`, or `?api_key=...` for WebSocket clients. Prefer headers when possible; use `?api_key=...` only when a browser WebSocket client cannot set headers, because query strings may be captured by proxy/access logs.

Check the API:

```bash
curl http://localhost:8080/health
```

Open the public moderation policy page:

```text
http://localhost:8080/moderation
```

Stop the stack:

```bash
docker compose down
```

## Submit Content

`POST /v1/check` accepts a signed Nostr event. Send either `raw_event`, or send an `event_id` and Aedos will try to fetch the signed event from configured `NOSTR_RELAYS`.

```bash
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -d '{
    "raw_event": {
      "id": "...",
      "pubkey": "...",
      "kind": 1,
      "content": "look https://example.com/image.png",
      "tags": [],
      "created_at": 1710000000,
      "sig": "..."
    }
  }'
```

Unsigned event/media associations are rejected. If `image_urls` or `video_urls` are supplied as hints, each URL must already be present in the signed event. Aedos extracts the event ID, pubkey, text tags, image URLs, and video URLs from the verified note before queueing media.

By default, `/v1/check` queues new media and returns immediately. The first response may be `unknown` while the worker downloads and reviews the media:

```json
{
  "type": "verdict",
  "event_id": "signed-event-id",
  "status": "unknown",
  "cache": false,
  "labels": ["unknown"],
  "confidence": 0.0
}
```

Later calls for the same event ID return the cached event verdict. New events with an already-seen image or video SHA-256 reuse the cached media verdict and do not call the AI provider again.

For a one-request flow, add `wait: true`. Aedos will queue the work and hold the HTTP request open until the event verdict is stored, or until the timeout is reached.

```bash
curl -X POST http://localhost:8080/v1/check \
  -H 'content-type: application/json' \
  -d '{
    "event_id": "signed-event-id-from-a-relay",
    "wait": true,
    "timeout_seconds": 30
  }'
```

`timeout_seconds` defaults to `30` and is clamped between `1` and `60`. If the timeout is reached before processing finishes, Aedos still returns `unknown`; a later check will return the cached verdict.

`POST /v1/submit` accepts a raw Nostr event. Aedos stores the event, verifies the event signature before attributing the author, extracts image/video URLs from the content, and checks text tags.

```bash
curl -X POST http://localhost:8080/v1/submit \
  -H 'content-type: application/json' \
  -d '{
    "raw_event": {
      "id": "...",
      "pubkey": "...",
      "kind": 1,
      "content": "hello #nsfw",
      "tags": [["t", "nsfw"]],
      "created_at": 1710000000
    }
  }'
```

## OpenAI Moderation

By default, Aedos uses the deterministic development provider. It does not call any external AI service.

To enable OpenAI:

```env
MODERATION_PROVIDER=openai
OPENAI_API_KEY=sk-...
OPENAI_MODERATION_MODEL=omni-moderation-latest
```

You can set those in `.env` before startup or in the dashboard settings page after setup. Dashboard settings are stored in Postgres and the Python worker hot-applies provider settings on its next queue loop.

Aedos refuses `MODERATION_PROVIDER=openai` unless `OPENAI_API_KEY` is present.

OpenAI responses are stored in a compact audit shape:

- response ID
- model
- `flagged`
- categories
- category scores
- category input-type map

The full image or video is not stored in the database. For videos, Aedos stores the video hash and metadata, then sends sampled frames for review.

## Verdict Labels

Supported labels currently include:

```text
safe
nsfw
nudity
sexual
sexualised
graphic
gore
violence
weapon
self-harm
hate-symbol
spam
scam
csam-suspected
unknown
```

OpenAI category mapping includes:

- `sexual/minors` -> `csam-suspected`
- `sexual` -> `nsfw`, `sexual`
- high sexual score without a category flag -> `sexualised`
- `violence` -> `violence`
- `violence/graphic` -> `graphic`, `gore`
- self-harm categories -> `self-harm`
- hate categories -> `hate-symbol`
- illicit categories -> `scam`

`csam-suspected` is treated as a block verdict and creates an emergency escalation metadata row. Operators still need a real legal/process path before using that signal in production.

## Nostr Label Events

The interoperable Nostr verdict format is NIP-32 Labeling.

Aedos builds label event drafts like this:

```json
{
  "kind": 1985,
  "tags": [
    ["L", "nostr.com/moderation"],
    ["l", "nsfw", "nostr.com/moderation"],
    ["l", "sexual", "nostr.com/moderation"],
    ["e", "<event-id>"]
  ],
  "content": "{\"status\":\"warn\",\"confidence\":0.85,\"source\":\"openai_moderation\",\"explanation\":\"OpenAI moderation flagged image categories\"}"
}
```

Target tags:

- `["e", "<event-id>"]` for event verdicts
- `["p", "<hex-pubkey>"]` for author/pubkey verdicts
- `["r", "<url>"]` for URL verdicts
- `["x", "<sha256>"]` for image and video hash verdicts

When `ENABLE_LABEL_PUBLISHER=true`, `NOSTR_PRIVATE_KEY` is set, and `NOSTR_RELAYS` contains at least one relay, the Rust API process scans final stored event verdicts and publishes NIP-32 label events in the background. Published label drafts and their Nostr event IDs are recorded in `published_labels` to avoid repeat publishing.

If `LABEL_NAMESPACE` is not explicitly set, Aedos derives it from `PUBLIC_BASE_URL`:

```text
PUBLIC_BASE_URL=https://your-aedos.example
LABEL_NAMESPACE=https://your-aedos.example/moderation
```

That URL is served by Aedos as a public, non-secret moderation policy page explaining what the instance does, what provider class it uses, which labels it emits, and how its tag rules work.

There is also a configurable realtime event draft kind, `ORACLE_VERDICT_KIND`, defaulting to `31494`. That is Aedos-specific and useful for direct integrations, but NIP-32 kind `1985` is the standards-aligned format clients and relays should prefer.

## Relay And Client Integration

Relays and clients can use Aedos in two ways:

- Query the WebSocket API directly when they see an event, which is the recommended active integration path because it can submit checks and receive later verdict updates on the same connection.
- Use HTTP for one-off checks, simple server integrations, or fallback paths.
- Consume NIP-32 label events from trusted Aedos label pubkeys.

For a copy/paste handoff to another coding agent, use [docs/INTEGRATION_AGENT_PROMPT.md](docs/INTEGRATION_AGENT_PROMPT.md). It explains the expected relay/client behavior, API shapes, fallback policies, NIP-32 label consumption, and tests an integration should add.

Recommended relay behavior:

- Use `POST /v1/check` with `wait=true` if the relay needs a verdict before storing or sharing an event.
- Use `/v1/ws` if the relay can queue or quarantine unknown media and apply the final verdict when it arrives.
- Treat `safe` as accept, `warn` as configurable mark/blur/downrank, `block` as reject/hide, and `unknown`/`error` according to the relay operator's fallback policy.

Recommended client behavior:

- Prefer verified NIP-32 labels from trusted Aedos pubkeys when available.
- Query Aedos only when no trusted label is available or when the client wants a fresher check.
- Blur/collapse `warn` media by default, hide `block` media, and make `unknown` behavior user-configurable.

## Public Page And Dashboard

The SvelteKit app runs at:

```text
http://localhost:3000
```

Routes:

- `/`: public Aedos explainer page.
- `/login`: operator login and first-install admin setup.
- `/admin`: authenticated control surface.

The admin dashboard includes:

- First-install admin setup.
- Login/logout using server-side sessions.
- Overview stats for processed media, daily volume, queue depth, retries, dead letters, and status counts.
- Nostr relay connectivity checks with online/offline indicators.
- Searchable, paginated image/video table.
- Processing/retry/failed job status.
- Job error details when a fetch or provider call fails.
- Review modal for changing verdicts.
- `Recheck with AI` action for forcing a fresh provider review.
- Provider response details for OpenAI audit data.
- Settings page with masked secrets and explanatory hints.
- Light/dark theme toggle stored in localStorage.

Settings are stored in Postgres. Secret settings are masked when read back.

Hot-applied worker/provider settings:

- `MODERATION_PROVIDER`
- `OPENAI_API_KEY`
- `OPENAI_MODERATION_MODEL`
- `MAX_IMAGE_BYTES`
- `MAX_VIDEO_BYTES`
- `IMAGE_FETCH_TIMEOUT_SECONDS`
- `MAX_VIDEO_FRAMES`
- `VIDEO_FRAME_INTERVAL_SECONDS`
- `QUEUE_STREAM_MAXLEN`
- `QUEUE_DEAD_LETTER_MAXLEN`

Public API rate limiting is controlled by:

- `RATE_LIMIT_CHECKS_PER_MINUTE`

Text tag review is controlled by:

- `TEXT_MARKERS_NSFW`
- `TEXT_MARKERS_CSAM`

These are comma, whitespace, or newline separated marker lists. They match only note hashtags such as `#nsfw` and Nostr topic tags such as `["t", "nsfw"]`; they do not match plain text sentences that merely mention a word.

Boot-level settings still require restarting the relevant service after editing `.env`, such as database URLs, Redis URLs, bind ports, and Compose port mappings.

## Production Hardening

Before exposing Aedos outside a trusted network:

- Set `API_KEYS` to one or more long random keys.
- Set `POSTGRES_PASSWORD` to a long random password before first production boot. If you already started with the default password, recreate the Postgres volume after changing it.
- Keep `PUBLISHED_BIND_HOST=127.0.0.1` and put public access behind a reverse proxy, SSH tunnel, or firewall rule. Do not expose Postgres or Redis to the public internet.
- Set `ALLOWED_ORIGINS` to the dashboard/client origins that should use the API from browsers.
- Put Aedos behind HTTPS and set `SECURE_COOKIES=true`.
- Set `NOSTR_PRIVATE_KEY` to the signing key for your oracle.
- Set `NOSTR_RELAYS` to the relays where labels should be published.
- Keep `ENABLE_LABEL_PUBLISHER=true` if you want stored event verdicts published as NIP-32 labels.
- Run all test suites before deploying changes.
- Put Postgres and Redis on private networking with real credentials.

## Author Lists

Aedos can return authors whose stored event verdicts include NSFW or CSAM-suspected labels.

```text
GET /v1/npubs/nsfw
GET /v1/npubs/csam
```

Add `?min_events=2` to return only authors with repeated matching events:

```text
GET /v1/npubs/nsfw?min_events=2
GET /v1/npubs/csam?min_events=2
```

Responses include the active `min_events` filter, hex pubkeys, bech32 `npub` values when valid, event counts, recent event IDs, and the latest matching time.

These lists are derived from stored verdicts and verified signed event/pubkey links. Aedos does not trust caller-supplied event/media/author associations for author-list membership. They are not external blocklists.

## Queue Reliability

Analysis jobs are stored in Redis:

- `oracle:analysis`: active analysis stream
- `oracle:analysis:retry`: delayed retry set
- `oracle:analysis:dead`: dead-letter stream

Workers acknowledge jobs only after successful processing. Failed jobs are retried with exponential backoff and then moved to the dead-letter stream after the retry limit is reached. Stream sizes are capped with Redis `MAXLEN` settings so busy deployments do not grow without bounds.

The dashboard also stores per-media job state in Postgres so operators can see whether a media item is queued, processing, retrying, completed, or failed.

## Operations

Backup, retention, migration, monitoring, alerting, and recovery procedures are defined in [docs/OPERATIONS.md](docs/OPERATIONS.md).

Included helpers:

- `scripts/backup-postgres.sh`
- `scripts/restore-postgres.sh`
- `scripts/retention.sql`
- `monitoring/prometheus-alerts.yml`

## Data Storage

Aedos stores:

- Event IDs, optional pubkeys, content, and raw event JSON for submitted events.
- Image/video URLs, normalized URLs, SHA-256 hashes, metadata, and image perceptual hashes.
- Event-to-media links.
- Verdicts with status, labels, confidence, source, model version, explanation, and compact provider response.
- Emergency escalation metadata for `csam-suspected`.
- Dashboard users, sessions, settings, and rate-limit counters.

Aedos does not store image or video bytes in Postgres.

## API

Public API:

- `POST /v1/check`
- `POST /v1/check_batch`
- `POST /v1/submit`
- `GET /v1/event/:event_id`
- `GET /v1/image/:sha256`
- `GET /v1/video/:sha256`
- `GET /v1/npubs/nsfw`
- `GET /v1/npubs/csam`
- `GET /v1/ws`
- `GET /v1/ws/firehose`
- `GET /moderation`
- `GET /health`
- `GET /metrics`

Dashboard API:

- `GET/POST /admin/api/setup`
- `POST /admin/api/login`
- `POST /admin/api/logout`
- `GET /admin/api/session`
- `GET /admin/api/overview`
- `GET /admin/api/images`
- `POST /admin/api/images/:sha256/verdict`
- `POST /admin/api/images/:sha256/recheck`
- `POST /admin/api/videos/:sha256/verdict`
- `POST /admin/api/videos/:sha256/recheck`
- `GET/POST /admin/api/settings`

WebSocket check:

```json
{"type":"check","raw_event":{"id":"...","pubkey":"...","kind":1,"content":"https://example.com/a.png","tags":[],"created_at":1710000000,"sig":"..."}}
```

WebSocket batch check:

```json
{"type":"check_batch","events":[{"event_id":"signed-event-id-to-fetch-from-relays"}]}
```

The WebSocket returns the current verdict immediately. If that verdict is `unknown` and the request queued media for review, the connection stays subscribed to that event ID and sends another `verdict` message when the worker stores the final result. With Postgres enabled, worker/API verdict writes notify connected WebSockets through `LISTEN/NOTIFY`; a short polling loop remains as a fallback.

You can also subscribe to existing event IDs without queueing new media:

```json
{"type":"subscribe","event_ids":["event1","event2"]}
```

Stop watching event IDs:

```json
{"type":"unsubscribe","event_ids":["event1"]}
```

Fetch repeat-sharing author lists over WebSocket:

```json
{"type":"author_list","list":"nsfw","min_events":2,"limit":1000}
```

`list` can be `nsfw` or `csam`. `min_events` defaults to `1`.

### Firehose WebSocket

Trusted peers can connect to `/v1/ws/firehose` to receive every completed event verdict as it is stored. This endpoint requires `API_KEYS` to be configured and the caller must provide a key with `x-api-key`, `Authorization: Bearer ...`, or `?api_key=...`.

```text
ws://localhost:8080/v1/ws/firehose?api_key=your-key
```

On connect, Aedos sends:

```json
{"type":"firehose_ready","scope":"event_verdicts"}
```

Then each completed event verdict is sent as:

```json
{
  "type": "firehose_verdict",
  "target_type": "event",
  "target_id": "event-id",
  "verdict": {
    "type": "verdict",
    "event_id": "event-id",
    "status": "warn",
    "cache": true,
    "labels": ["nsfw"],
    "confidence": 0.91
  }
}
```

Use the normal `/v1/ws` endpoint for scoped relay/client checks. Use the firehose only for trusted operators or friends who should see all Aedos verdict activity.

## Environment

See `.env.example` for defaults. Important values:

- `DATABASE_URL`
- `POSTGRES_PASSWORD`
- `PUBLISHED_BIND_HOST`
- `POSTGRES_PORT`
- `REDIS_PORT`
- `ORACLE_PORT`
- `DASHBOARD_PORT`
- `PUBLIC_ORACLE_BASE_URL`
- `REDIS_URL`
- `NOSTR_PRIVATE_KEY`
- `NOSTR_RELAYS`
- `PUBLIC_BASE_URL`
- `ALLOWED_ORIGINS`
- `SECURE_COOKIES`
- `LABEL_NAMESPACE`
- `ENABLE_LABEL_PUBLISHER`
- `LABEL_PUBLISH_INTERVAL_SECONDS`
- `ORACLE_VERDICT_KIND`
- `MAX_IMAGE_BYTES`
- `MAX_VIDEO_BYTES`
- `IMAGE_FETCH_TIMEOUT_SECONDS`
- `MAX_VIDEO_FRAMES`
- `VIDEO_FRAME_INTERVAL_SECONDS`
- `WORKER_CONCURRENCY`
- `QUEUE_CONSUMER_GROUP`
- `QUEUE_CONSUMER_NAME`
- `QUEUE_STREAM_MAXLEN`
- `QUEUE_DEAD_LETTER_MAXLEN`
- `RATE_LIMIT_CHECKS_PER_MINUTE`
- `TEXT_MARKERS_NSFW`
- `TEXT_MARKERS_CSAM`
- `MODERATION_PROVIDER`
- `OPENAI_API_KEY`
- `OPENAI_MODERATION_MODEL`
- `API_KEYS`

`NOSTR_PRIVATE_KEY` is for signing moderation label events so clients and relays can verify that labels came from your Aedos instance.

`NOSTR_RELAYS` are the relays Aedos is configured to use for Nostr label delivery. The dashboard also uses them to show WebSocket connectivity.

The default relays are `wss://relay.nostr.com`, `wss://relay.damus.io`, `wss://nos.lol`, `wss://nostr.bitcoiner.social`, `wss://nostr.mom`, and `wss://relay.snort.social`.

## Tests

Rust:

```bash
cargo test
```

Python:

```bash
cd workers/python
uv run pytest
```

Dashboard:

```bash
cd apps/dashboard
npm install
npm run check
```
