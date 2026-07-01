from __future__ import annotations

import asyncio
import json
import os
import uuid
from io import BytesIO
from typing import Any

import asyncpg
import httpx
import redis.asyncio as redis
from PIL import Image

from hashing import fingerprint_image
from moderation import DeterministicModerationModel, ModerationModel, Verdict


JOB_QUEUE = "oracle:analysis"


def env_int(name: str, default: int) -> int:
    try:
        return int(os.getenv(name, str(default)))
    except ValueError:
        return default


async def fetch_image(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
    async with httpx.AsyncClient(follow_redirects=True, timeout=timeout_seconds) as client:
        async with client.stream("GET", url) as response:
            response.raise_for_status()
            mime_type = response.headers.get("content-type", "application/octet-stream").split(";")[0]
            chunks: list[bytes] = []
            total = 0
            async for chunk in response.aiter_bytes():
                total += len(chunk)
                if total > max_bytes:
                    raise ValueError("image exceeds MAX_IMAGE_BYTES")
                chunks.append(chunk)
            return b"".join(chunks), mime_type


async def store_verdict(conn: asyncpg.Connection, event_id: str, verdict: Verdict) -> None:
    await conn.execute(
        """
        insert into verdicts
        (id, target_type, target_id, status, safe, warn, block, unknown, error, labels,
         confidence, source, cache, model_version, explanation)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::jsonb,$11,$12,$13,$14,$15)
        """,
        uuid.uuid4(),
        "event",
        event_id,
        verdict.status,
        verdict.safe,
        verdict.warn,
        verdict.block,
        verdict.status == "unknown",
        verdict.status == "error",
        json.dumps(verdict.labels),
        verdict.confidence,
        verdict.source,
        False,
        verdict.model_version,
        verdict.explanation,
    )


async def store_emergency_escalation(
    conn: asyncpg.Connection,
    *,
    event_id: str,
    normalized_url: str,
    sha256: str,
    verdict: Verdict,
) -> None:
    await conn.execute(
        """
        insert into emergency_escalations
        (id, event_id, image_sha256, normalized_url, label, status, confidence, source,
         model_version, explanation)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
        """,
        uuid.uuid4(),
        event_id,
        sha256,
        normalized_url,
        "csam-suspected",
        "pending_operator_review",
        verdict.confidence,
        verdict.source,
        verdict.model_version,
        verdict.explanation,
    )


async def process_job(
    conn: asyncpg.Connection,
    job: dict[str, Any],
    model: ModerationModel,
    max_bytes: int,
    timeout_seconds: int,
) -> None:
    event_id = job["event_id"]
    for url in job.get("image_urls", []):
        payload, mime_type = await fetch_image(url, max_bytes=max_bytes, timeout_seconds=timeout_seconds)
        fingerprint = fingerprint_image(payload, mime_type)

        image = Image.open(BytesIO(payload))
        verdict = model.analyse(image)

        await conn.execute(
            """
            insert into images
            (id, url, normalized_url, sha256, phash, mime_type, width, height, bytes)
            values ($1,$2,$3,$4,$5,$6,$7,$8,$9)
            on conflict (sha256) do nothing
            """,
            uuid.uuid4(),
            url,
            url,
            fingerprint.sha256,
            fingerprint.phash,
            fingerprint.mime_type,
            fingerprint.width,
            fingerprint.height,
            fingerprint.bytes,
        )
        await store_verdict(conn, event_id, verdict)
        if verdict.requires_emergency_escalation:
            await store_emergency_escalation(
                conn,
                event_id=event_id,
                normalized_url=url,
                sha256=fingerprint.sha256,
                verdict=verdict,
            )


async def run_worker() -> None:
    redis_url = os.getenv("REDIS_URL", "redis://localhost:6379/0")
    database_url = os.getenv("DATABASE_URL", "postgresql://oracle:oracle@localhost:5432/oracle")
    max_bytes = env_int("MAX_IMAGE_BYTES", 10_000_000)
    timeout_seconds = env_int("IMAGE_FETCH_TIMEOUT_SECONDS", 10)

    redis_client = redis.from_url(redis_url, decode_responses=True)
    conn = await asyncpg.connect(database_url)
    model = DeterministicModerationModel()

    while True:
        _, payload = await redis_client.blpop(JOB_QUEUE)
        await process_job(conn, json.loads(payload), model, max_bytes, timeout_seconds)


if __name__ == "__main__":
    asyncio.run(run_worker())
