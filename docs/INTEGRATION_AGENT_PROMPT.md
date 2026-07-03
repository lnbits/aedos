# Aedos Integration Agent Prompt

Use this prompt when asking a coding agent to integrate a Nostr relay or client with Aedos.

```text
You are integrating a Nostr relay or client with Aedos, an AI-powered moderation oracle for Nostr events, text, images, and videos.

Goal:
Add Aedos moderation support without making Aedos a hard dependency. If Aedos is unavailable, the relay/client should follow its configured fallback policy.

Important Aedos concepts:
- Aedos reviews Nostr event IDs, optional pubkeys, text/tags, image URLs, and video URLs.
- Aedos returns compact verdicts: safe, warn, block, unknown, or error.
- Labels may include safe, nsfw, nudity, sexual, sexualised, graphic, gore, violence, weapon, self-harm, hate-symbol, spam, scam, csam-suspected, and unknown.
- Aedos caches verdicts by event ID and media SHA-256. Do not re-send unchanged content in a tight loop.
- Prefer the Aedos WebSocket API for active relay/client integrations. It lets the integration submit checks and receive final verdict updates on the same connection.
- Use HTTP for simple one-off checks, startup probes, server environments where WebSockets are awkward, or fallback behavior.
- Aedos can return unknown immediately while media is queued. Use WebSockets, or HTTP wait mode, if the integration needs the final verdict before accepting/displaying content.
- If API_KEYS is configured on Aedos, pass the key as x-api-key, Authorization: Bearer <key>, or ?api_key=<key> for WebSocket connections.
- Aedos can also publish NIP-32 label events, kind 1985, to configured Nostr relays. Prefer verified NIP-32 labels for clients/relays that already consume Nostr label events.

HTTP endpoint:
POST {AEDOS_URL}/v1/check
Content-Type: application/json

Request shape:
{
  "event_id": "<nostr event id>",
  "npub": "<optional npub or hex pubkey>",
  "image_urls": ["https://example.com/image.jpg"],
  "video_urls": ["https://example.com/video.mp4"],
  "wait": false,
  "timeout_seconds": 30
}

Notes:
- event_id is required.
- npub/pubkey, image_urls, and video_urls are optional extras.
- Use wait=true when the relay/client needs a final verdict before taking action.
- timeout_seconds is clamped by Aedos. Treat timeout/unknown as policy-controlled, not as safe.

HTTP response shape:
{
  "type": "verdict",
  "event_id": "<event id>",
  "status": "safe|warn|block|unknown|error",
  "cache": true,
  "labels": ["safe"],
  "confidence": 0.91
}

WebSocket endpoint:
GET {AEDOS_WS_URL}/v1/ws

Preferred WebSocket flow:
1. Open one bounded, long-lived WebSocket connection to Aedos.
2. Send check or check_batch messages as relay/client events arrive.
3. Apply any immediate safe/warn/block verdict.
4. If Aedos returns unknown, keep the event pending, quarantined, blurred, or temporarily rejected according to local policy.
5. Keep the socket open and apply the later verdict message when Aedos finishes processing.
6. Reconnect with backoff and resubscribe to any still-pending event IDs.

Send one event:
{"type":"check","event_id":"<event id>","npub":"<optional pubkey>","image_urls":["https://example.com/a.jpg"],"video_urls":["https://example.com/a.mp4"]}

Send a batch:
{"type":"check_batch","events":[{"event_id":"<event id>","npub":"<optional pubkey>","image_urls":[],"video_urls":[]}]}

Subscribe without queueing new media:
{"type":"subscribe","event_ids":["<event id>"]}

Expected WebSocket behavior:
- Aedos returns the current verdict immediately.
- If the verdict is unknown and media was queued, keep the socket open. Aedos will send a later verdict message when processing finishes.
- Reconnect with backoff. Do not create an unbounded reconnect loop.

Relay integration behavior:
1. On EVENT received, extract:
   - event id
   - pubkey
   - content
   - image URLs
   - direct video URLs
2. Submit the event to Aedos.
   - Prefer /v1/ws for submit-and-receive-updates behavior.
   - If your relay must decide before storing and cannot keep a pending queue, use HTTP wait=true with a short timeout.
   - If your relay can quarantine/pending-store, accept into a pending state and consume the later WebSocket verdict.
3. Apply policy:
   - safe: accept/store/share normally.
   - warn: store or share according to local policy; clients may blur, mark, or downrank.
   - block: reject, hide, or avoid sharing the event. Do not store media bytes just for investigation.
   - unknown/error: use the relay operator's fallback, usually reject, quarantine, or blur.
4. For csam-suspected:
   - Do not publish, mirror, cache, or display the media.
   - Treat the signal as requiring the relay/operator's own legal and incident-response process.
   - Do not build a human-review flow that casually exposes suspected illegal media.
5. Keep Aedos optional:
   - Add config for AEDOS_URL, AEDOS_API_KEY, AEDOS_TIMEOUT_SECONDS, and AEDOS_DEFAULT_POLICY.
   - Make the integration easy to disable.
   - Record enough logs to debug Aedos failures without logging sensitive media payloads.

Client integration behavior:
1. For notes in timelines, extract event id, pubkey, image URLs, and direct video URLs.
2. Prefer existing NIP-32 labels from trusted Aedos label pubkeys when available.
3. If no trusted label exists, query Aedos using WebSockets for timeline/live updates. Use HTTP for simple fallback checks.
4. Apply display policy:
   - safe: render normally.
   - warn labels like nsfw, nudity, sexual, sexualised, graphic, gore: blur/collapse behind a user action.
   - block or csam-suspected: do not render media or share the event onward.
   - unknown/error: use the user's safety setting, defaulting to blur/collapse.
5. Cache verdicts locally by event ID and media URL/hash if available. Respect Aedos cache results and avoid repeated checks while scrolling.
6. Never send private drafts or encrypted content to Aedos unless the user explicitly opted in.

NIP-32 label consumption:
- Aedos publishes kind 1985 label events when configured.
- Verify the label event signature.
- Trust only configured Aedos label pubkeys.
- Read labels from ["l", "<label>", "<namespace>"] tags.
- Event verdicts target ["e", "<event-id>"].
- Author verdicts target ["p", "<hex-pubkey>"].
- URL verdicts target ["r", "<url>"].
- Media hash verdicts target ["x", "<sha256>"].
- The default namespace is nostr.com/moderation unless changed with LABEL_NAMESPACE.

Implementation requirements:
- Add tests for safe, warn, block, unknown, timeout, Aedos unavailable, and API-key configured cases.
- Add a config example.
- Avoid storing image/video bytes in the relay/client database.
- Avoid blocking the whole relay/client event loop while waiting for Aedos.
- Use bounded concurrency, request timeouts, and backoff.
- Keep the policy configurable because different relays/clients may want different handling for warn/unknown.

Deliverables:
- Code integration.
- Config documentation.
- Tests.
- A short operator/client-user note explaining what Aedos does and what the fallback policy is.
```

## Minimal HTTP Example

```bash
curl -X POST "$AEDOS_URL/v1/check" \
  -H 'content-type: application/json' \
  -H "x-api-key: $AEDOS_API_KEY" \
  -d '{
    "event_id": "example-event",
    "npub": "npub1...",
    "image_urls": ["https://example.com/image.jpg"],
    "video_urls": [],
    "wait": true,
    "timeout_seconds": 20
  }'
```

## Suggested Relay Config

```env
AEDOS_ENABLED=true
AEDOS_URL=http://localhost:8080
AEDOS_API_KEY=
AEDOS_TIMEOUT_SECONDS=20
AEDOS_DEFAULT_POLICY=reject_unknown
AEDOS_WARN_POLICY=mark
AEDOS_BLOCK_POLICY=reject
```

## Suggested Client Config

```env
AEDOS_ENABLED=true
AEDOS_URL=https://aedos.example.com
AEDOS_API_KEY=
AEDOS_DEFAULT_POLICY=blur_unknown
AEDOS_TRUSTED_LABEL_PUBKEYS=<hex-pubkey>
```
