from __future__ import annotations

import asyncio
import json
import os
import socket
import tempfile
import time
import uuid
import hashlib
from io import BytesIO
from pathlib import Path
from typing import Any

import asyncpg
import httpx
import redis.asyncio as redis
from redis.exceptions import ResponseError, TimeoutError as RedisTimeoutError
from PIL import Image

from hashing import fingerprint_image
from moderation import ModerationModel, Verdict
from providers import create_moderation_model


JOB_QUEUE = "oracle:analysis"
RETRY_QUEUE = "oracle:analysis:retry"
DEAD_LETTER_QUEUE = "oracle:analysis:dead"
QUEUE_GROUP = "aedos-workers"
QUEUE_CONSUMER = f"{socket.gethostname()}:{os.getpid()}"
QUEUE_POLL_SECONDS = 5
MAX_JOB_ATTEMPTS = 5
BASE_RETRY_SECONDS = 10
MAX_RETRY_SECONDS = 300
PENDING_IDLE_MS = 60_000
RECOVER_PENDING_COUNT = 10
DEFAULT_STREAM_MAXLEN = 1_000_000
DEFAULT_DEAD_LETTER_MAXLEN = 100_000
IMAGE_FETCH_HEADERS = {
    "Accept": "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
    "User-Agent": (
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
    ),
}
VIDEO_FETCH_HEADERS = {
    "Accept": "video/mp4,video/webm,video/*,*/*;q=0.8",
    "User-Agent": IMAGE_FETCH_HEADERS["User-Agent"],
}
DEFAULT_MAX_VIDEO_BYTES = 50_000_000
DEFAULT_MAX_VIDEO_FRAMES = 8
DEFAULT_VIDEO_FRAME_INTERVAL_SECONDS = 5
RUNTIME_SETTING_KEYS = {
    "MAX_IMAGE_BYTES",
    "MAX_VIDEO_BYTES",
    "IMAGE_FETCH_TIMEOUT_SECONDS",
    "MAX_VIDEO_FRAMES",
    "VIDEO_FRAME_INTERVAL_SECONDS",
    "QUEUE_STREAM_MAXLEN",
    "QUEUE_DEAD_LETTER_MAXLEN",
    "MODERATION_PROVIDER",
    "OPENAI_API_KEY",
    "OPENAI_MODERATION_MODEL",
}
PROVIDER_SETTING_KEYS = {
    "MODERATION_PROVIDER",
    "OPENAI_API_KEY",
    "OPENAI_MODERATION_MODEL",
}


class FetchImageError(RuntimeError):
    def __init__(self, message: str, *, retryable: bool = True) -> None:
        super().__init__(message)
        self.retryable = retryable


class FetchVideoError(FetchImageError):
    pass


class VideoFrameExtractionError(RuntimeError):
    retryable = False


def env_int(name: str, default: int) -> int:
    try:
        return int(os.getenv(name, str(default)))
    except ValueError:
        return default


def setting_str(settings: dict[str, str], name: str, default: str = "") -> str:
    return settings.get(name) or os.getenv(name, default)


def setting_int(settings: dict[str, str], name: str, default: int) -> int:
    try:
        return int(setting_str(settings, name, str(default)))
    except ValueError:
        return default


def env_str(name: str, default: str) -> str:
    return os.getenv(name, default).strip() or default


def job_attempts(job: dict[str, Any]) -> int:
    try:
        return int(job.get("attempts", 0))
    except (TypeError, ValueError):
        return 0


def retry_delay_seconds(attempts: int) -> int:
    delay = BASE_RETRY_SECONDS * (2 ** max(attempts - 1, 0))
    return min(delay, MAX_RETRY_SECONDS)


def with_failure_metadata(job: dict[str, Any], error: Exception) -> dict[str, Any]:
    updated = dict(job)
    updated["attempts"] = job_attempts(job) + 1
    updated["last_error"] = error_message(error)
    updated["last_error_type"] = type(error).__name__
    updated["last_failed_at"] = int(time.time())
    return updated


def error_message(error: Exception | str) -> str:
    if isinstance(error, str):
        return error
    message = str(error).strip()
    if message:
        return message
    cause = getattr(error, "__cause__", None)
    if cause is not None:
        cause_message = str(cause).strip()
        if cause_message:
            return f"{type(error).__name__}: {cause_message}"
    return type(error).__name__


def optional_row_value(row: Any, key: str) -> Any:
    try:
        return row[key]
    except (KeyError, IndexError):
        return None


async def xadd_payload(redis_client: redis.Redis, stream: str, payload: str, *, maxlen: int) -> None:
    await redis_client.xadd(stream, {"payload": payload}, maxlen=maxlen, approximate=True)


async def load_runtime_settings(conn: asyncpg.Connection) -> dict[str, str]:
    try:
        rows = await conn.fetch(
            """
            select key, value
            from admin_settings
            where key = any($1::text[])
            """,
            sorted(RUNTIME_SETTING_KEYS),
        )
    except asyncpg.UndefinedTableError:
        return {}
    return {row["key"]: row["value"] for row in rows}


async def ensure_image_jobs_schema(conn: asyncpg.Connection) -> None:
    await conn.execute("alter table if exists verdicts add column if not exists provider_response jsonb")
    await conn.execute(
        """
        create table if not exists videos (
          id uuid primary key,
          url text not null,
          normalized_url text not null,
          sha256 text unique,
          mime_type text,
          bytes integer,
          first_seen_at timestamptz not null default now()
        )
        """
    )
    await conn.execute("create index if not exists videos_normalized_url_idx on videos (normalized_url)")
    await conn.execute(
        """
        create table if not exists event_videos (
          event_id text not null references events(id) on delete cascade,
          video_id uuid not null references videos(id) on delete cascade,
          primary key (event_id, video_id)
        )
        """
    )
    await conn.execute(
        """
        create table if not exists image_jobs (
          sha256 text primary key,
          status text not null,
          last_error text,
          queued_at timestamptz not null default now(),
          started_at timestamptz,
          finished_at timestamptz,
          updated_at timestamptz not null default now()
        )
        """
    )
    await conn.execute(
        """
        create table if not exists analysis_jobs (
          job_key text primary key,
          event_id text not null,
          url text not null,
          media_type text not null default 'image',
          image_sha256 text,
          status text not null,
          last_error text,
          queued_at timestamptz not null default now(),
          started_at timestamptz,
          finished_at timestamptz,
          updated_at timestamptz not null default now()
        )
        """
    )
    await conn.execute("alter table if exists analysis_jobs add column if not exists media_type text not null default 'image'")


async def update_image_job(
    conn: asyncpg.Connection,
    *,
    sha256: str | None,
    status: str,
    error: Exception | str | None = None,
) -> None:
    if not sha256:
        return
    error_text = error_message(error) if error else None
    await conn.execute(
        """
        insert into image_jobs (sha256, status, last_error, queued_at, started_at, finished_at, updated_at)
        values (
          $1,
          $2,
          $3,
          now(),
          case when $2 = 'processing' then now() else null end,
          case when $2 in ('completed', 'failed') then now() else null end,
          now()
        )
        on conflict (sha256) do update set
          status = excluded.status,
          last_error = excluded.last_error,
          started_at = case
            when excluded.status = 'processing' then now()
            else image_jobs.started_at
          end,
          finished_at = case
            when excluded.status in ('completed', 'failed') then now()
            else null
          end,
          updated_at = now()
        """,
        sha256,
        status,
        error_text,
    )
    await conn.execute("select pg_notify('aedos_media', $1)", sha256)


def analysis_job_key(event_id: str, url: str) -> str:
    return hashlib.sha256(f"{event_id}\n{url}".encode("utf-8")).hexdigest()


async def update_analysis_job(
    conn: asyncpg.Connection,
    *,
    event_id: str,
    url: str,
    status: str,
    media_type: str = "image",
    image_sha256: str | None = None,
    error: Exception | str | None = None,
) -> None:
    error_text = error_message(error) if error else None
    await conn.execute(
        """
        insert into analysis_jobs
          (job_key, event_id, url, media_type, image_sha256, status, last_error, queued_at, started_at, finished_at, updated_at)
        values (
          $1,
          $2,
          $3,
          $4,
          $5,
          $6,
          $7,
          now(),
          case when $6 = 'processing' then now() else null end,
          case when $6 in ('completed', 'failed') then now() else null end,
          now()
        )
        on conflict (job_key) do update set
          media_type = excluded.media_type,
          image_sha256 = coalesce(excluded.image_sha256, analysis_jobs.image_sha256),
          status = excluded.status,
          last_error = excluded.last_error,
          started_at = case
            when excluded.status = 'processing' then now()
            else analysis_jobs.started_at
          end,
          finished_at = case
            when excluded.status in ('completed', 'failed') then now()
            else null
          end,
          updated_at = now()
        """,
        analysis_job_key(event_id, url),
        event_id,
        url,
        media_type,
        image_sha256,
        status,
        error_text,
    )
    await conn.execute("select pg_notify('aedos_media', $1)", image_sha256 or url)


def provider_signature(settings: dict[str, str]) -> tuple[str, str, str]:
    return tuple(setting_str(settings, key).strip() for key in sorted(PROVIDER_SETTING_KEYS))


async def fetch_media(
    url: str,
    *,
    max_bytes: int,
    timeout_seconds: int,
    headers: dict[str, str],
    media_name: str,
    error_type: type[FetchImageError],
) -> tuple[bytes, str]:
    try:
        async with httpx.AsyncClient(
            follow_redirects=True,
            headers=headers,
            timeout=timeout_seconds,
        ) as client:
            async with client.stream("GET", url) as response:
                response.raise_for_status()
                mime_type = response.headers.get("content-type", "application/octet-stream").split(";")[0]
                chunks: list[bytes] = []
                total = 0
                async for chunk in response.aiter_bytes():
                    total += len(chunk)
                    if total > max_bytes:
                        raise ValueError(f"{media_name} exceeds max configured bytes")
                    chunks.append(chunk)
                return b"".join(chunks), mime_type
    except Exception as exc:
        retryable = True
        if isinstance(exc, httpx.HTTPStatusError):
            status_code = exc.response.status_code
            retryable = status_code == 429 or status_code >= 500
        raise error_type(f"failed to fetch {media_name} {url}: {error_message(exc)}", retryable=retryable) from exc


async def fetch_image(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
    return await fetch_media(
        url,
        max_bytes=max_bytes,
        timeout_seconds=timeout_seconds,
        headers=IMAGE_FETCH_HEADERS,
        media_name="image",
        error_type=FetchImageError,
    )


async def fetch_video(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
    return await fetch_media(
        url,
        max_bytes=max_bytes,
        timeout_seconds=timeout_seconds,
        headers=VIDEO_FETCH_HEADERS,
        media_name="video",
        error_type=FetchVideoError,
    )


async def latest_verdict(
    conn: asyncpg.Connection,
    *,
    target_type: str,
    target_id: str,
) -> Verdict | None:
    row = await conn.fetchrow(
        """
        select status, labels, confidence, source, model_version, explanation, provider_response
        from verdicts
        where target_type = $1 and target_id = $2
        order by created_at desc
        limit 1
        """,
        target_type,
        target_id,
    )
    if row is None:
        return None

    labels = row["labels"]
    if isinstance(labels, str):
        labels = json.loads(labels)
    return Verdict(
        status=row["status"],
        labels=list(labels),
        confidence=float(row["confidence"]),
        source=row["source"],
        model_version=row["model_version"],
        explanation=row["explanation"],
        provider_response=optional_row_value(row, "provider_response"),
    )


async def store_verdict(
    conn: asyncpg.Connection,
    *,
    target_type: str,
    target_id: str,
    verdict: Verdict,
    cache: bool = False,
) -> None:
    await conn.execute(
        """
        insert into verdicts
        (id, target_type, target_id, status, safe, warn, block, unknown, error, labels,
         confidence, source, cache, model_version, explanation, provider_response)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::jsonb,$11,$12,$13,$14,$15,$16::jsonb)
        """,
        uuid.uuid4(),
        target_type,
        target_id,
        verdict.status,
        verdict.safe,
        verdict.warn,
        verdict.block,
        verdict.status == "unknown",
        verdict.status == "error",
        json.dumps(verdict.labels),
        verdict.confidence,
        verdict.source,
        cache,
        verdict.model_version,
        verdict.explanation,
        json.dumps(verdict.provider_response) if verdict.provider_response is not None else None,
    )
    if target_type == "event" and verdict.status != "unknown":
        await conn.execute("select pg_notify('aedos_verdicts', $1)", target_id)
    if target_type in {"image", "video"}:
        await conn.execute("select pg_notify('aedos_media', $1)", target_id)


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


async def store_event_stub(conn: asyncpg.Connection, *, event_id: str, pubkey: str | None = None) -> None:
    await conn.execute(
        """
        insert into events (id, pubkey, content, raw, created_at)
        values ($1, $2, '', '{}'::jsonb, extract(epoch from now())::bigint)
        on conflict (id) do update set
          pubkey = coalesce(excluded.pubkey, events.pubkey)
        """,
        event_id,
        pubkey,
    )


async def store_image_metadata(
    conn: asyncpg.Connection,
    *,
    url: str,
    fingerprint: Any,
) -> str:
    image_id = await conn.fetchval(
        """
        insert into images
        (id, url, normalized_url, sha256, phash, mime_type, width, height, bytes)
        values ($1,$2,$3,$4,$5,$6,$7,$8,$9)
        on conflict (sha256) do update set
          url = excluded.url,
          normalized_url = excluded.normalized_url,
          mime_type = excluded.mime_type,
          width = excluded.width,
          height = excluded.height,
          bytes = excluded.bytes
        returning id
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
    return str(image_id)


async def link_event_image(conn: asyncpg.Connection, *, event_id: str, image_id: str) -> None:
    await conn.execute(
        """
        insert into event_images (event_id, image_id)
        values ($1, $2)
        on conflict do nothing
        """,
        event_id,
        image_id,
    )


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


async def store_video_metadata(
    conn: asyncpg.Connection,
    *,
    url: str,
    sha256: str,
    mime_type: str,
    bytes_count: int,
) -> str:
    video_id = await conn.fetchval(
        """
        insert into videos
        (id, url, normalized_url, sha256, mime_type, bytes)
        values ($1,$2,$3,$4,$5,$6)
        on conflict (sha256) do update set
          url = excluded.url,
          normalized_url = excluded.normalized_url,
          mime_type = excluded.mime_type,
          bytes = excluded.bytes
        returning id
        """,
        uuid.uuid4(),
        url,
        url,
        sha256,
        mime_type,
        bytes_count,
    )
    return str(video_id)


async def link_event_video(conn: asyncpg.Connection, *, event_id: str, video_id: str) -> None:
    await conn.execute(
        """
        insert into event_videos (event_id, video_id)
        values ($1, $2)
        on conflict do nothing
        """,
        event_id,
        video_id,
    )


def video_suffix_for_mime_type(mime_type: str) -> str:
    return {
        "video/mp4": ".mp4",
        "video/webm": ".webm",
        "video/quicktime": ".mov",
        "video/x-m4v": ".m4v",
    }.get(mime_type, ".video")


async def extract_video_frames(
    payload: bytes,
    mime_type: str,
    *,
    max_frames: int,
    frame_interval_seconds: int,
) -> list[tuple[bytes, str]]:
    max_frames = max(1, max_frames)
    frame_interval_seconds = max(1, frame_interval_seconds)
    with tempfile.TemporaryDirectory(prefix="aedos-video-") as tmpdir:
        tmp = Path(tmpdir)
        input_path = tmp / f"input{video_suffix_for_mime_type(mime_type)}"
        output_pattern = tmp / "frame_%03d.jpg"
        input_path.write_bytes(payload)
        process = await asyncio.create_subprocess_exec(
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            str(input_path),
            "-vf",
            f"fps=1/{frame_interval_seconds}",
            "-frames:v",
            str(max_frames),
            str(output_pattern),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await process.communicate()
        if process.returncode != 0:
            message = stderr.decode("utf-8", errors="replace").strip() or "ffmpeg could not decode video"
            raise VideoFrameExtractionError(message)

        frames = [(path.read_bytes(), "image/jpeg") for path in sorted(tmp.glob("frame_*.jpg"))]
        if not frames:
            raise VideoFrameExtractionError("ffmpeg did not extract any video frames")
        return frames


def aggregate_video_verdict(frame_verdicts: list[Verdict]) -> Verdict:
    if not frame_verdicts:
        return Verdict(
            status="unknown",
            labels=["unknown"],
            confidence=0.0,
            source="video_frame_analysis",
            model_version=None,
            explanation="no video frames were available for review",
        )

    rank = {"safe": 0, "unknown": 1, "error": 2, "warn": 3, "block": 4}
    worst = max(frame_verdicts, key=lambda verdict: rank.get(verdict.status, 1))
    labels = sorted({label for verdict in frame_verdicts for label in verdict.labels if label != "safe"})
    if not labels:
        labels = ["safe"]
    confidence = max(verdict.confidence for verdict in frame_verdicts)
    return Verdict(
        status=worst.status,
        labels=labels,
        confidence=confidence,
        source=worst.source,
        model_version=worst.model_version,
        explanation=f"{len(frame_verdicts)} video frame(s) reviewed; highest-severity frame was {worst.status}",
        provider_response={
            "frame_count": len(frame_verdicts),
            "frames": [
                {
                    "index": index,
                    "status": verdict.status,
                    "labels": verdict.labels,
                    "confidence": verdict.confidence,
                    "source": verdict.source,
                    "model_version": verdict.model_version,
                    "explanation": verdict.explanation,
                    "provider_response": verdict.provider_response,
                }
                for index, verdict in enumerate(frame_verdicts)
            ],
        },
    )


async def process_job(
    conn: asyncpg.Connection,
    job: dict[str, Any],
    model: ModerationModel,
    max_bytes: int,
    timeout_seconds: int,
    *,
    max_video_bytes: int = DEFAULT_MAX_VIDEO_BYTES,
    max_video_frames: int = DEFAULT_MAX_VIDEO_FRAMES,
    video_frame_interval_seconds: int = DEFAULT_VIDEO_FRAME_INTERVAL_SECONDS,
) -> None:
    event_id = job["event_id"]
    force_recheck = bool(job.get("force_recheck"))
    image_only = bool(job.get("image_only"))
    if not image_only:
        await store_event_stub(conn, event_id=event_id, pubkey=job.get("pubkey"))
    for url in job.get("image_urls", []):
        await update_analysis_job(conn, event_id=event_id, url=url, status="processing")
        try:
            payload, mime_type = await fetch_image(url, max_bytes=max_bytes, timeout_seconds=timeout_seconds)
            fingerprint = fingerprint_image(payload, mime_type)
            image_id = await store_image_metadata(conn, url=url, fingerprint=fingerprint)
            await update_analysis_job(
                conn,
                event_id=event_id,
                url=url,
                status="processing",
                image_sha256=fingerprint.sha256,
            )
            if not image_only:
                await link_event_image(conn, event_id=event_id, image_id=image_id)

            verdict = None if force_recheck else await latest_verdict(conn, target_type="image", target_id=fingerprint.sha256)
            cache_hit = verdict is not None
            if verdict is None:
                image = Image.open(BytesIO(payload))
                verdict = await model.analyse(image, payload, mime_type)
                await store_verdict(
                    conn,
                    target_type="image",
                    target_id=fingerprint.sha256,
                    verdict=verdict,
                )

            if not image_only:
                await store_verdict(
                    conn,
                    target_type="event",
                    target_id=event_id,
                    verdict=verdict,
                    cache=cache_hit,
                )
            if verdict.requires_emergency_escalation:
                await store_emergency_escalation(
                    conn,
                    event_id=event_id,
                    normalized_url=url,
                    sha256=fingerprint.sha256,
                    verdict=verdict,
                )
            await update_analysis_job(
                conn,
                event_id=event_id,
                url=url,
                status="completed",
                image_sha256=fingerprint.sha256,
            )
        except Exception as exc:
            await update_analysis_job(conn, event_id=event_id, url=url, status="failed", error=exc)
            raise

    for url in job.get("video_urls", []):
        await update_analysis_job(conn, event_id=event_id, url=url, media_type="video", status="processing")
        try:
            payload, mime_type = await fetch_video(url, max_bytes=max_video_bytes, timeout_seconds=timeout_seconds)
            video_sha256 = sha256_bytes(payload)
            video_id = await store_video_metadata(
                conn,
                url=url,
                sha256=video_sha256,
                mime_type=mime_type,
                bytes_count=len(payload),
            )
            await update_analysis_job(
                conn,
                event_id=event_id,
                url=url,
                media_type="video",
                status="processing",
                image_sha256=video_sha256,
            )
            if not image_only:
                await link_event_video(conn, event_id=event_id, video_id=video_id)

            verdict = None if force_recheck else await latest_verdict(conn, target_type="video", target_id=video_sha256)
            cache_hit = verdict is not None
            if verdict is None:
                frames = await extract_video_frames(
                    payload,
                    mime_type,
                    max_frames=max_video_frames,
                    frame_interval_seconds=video_frame_interval_seconds,
                )
                frame_verdicts: list[Verdict] = []
                for frame_payload, frame_mime_type in frames:
                    image = Image.open(BytesIO(frame_payload))
                    frame_verdicts.append(await model.analyse(image, frame_payload, frame_mime_type))
                verdict = aggregate_video_verdict(frame_verdicts)
                await store_verdict(
                    conn,
                    target_type="video",
                    target_id=video_sha256,
                    verdict=verdict,
                )

            if not image_only:
                await store_verdict(
                    conn,
                    target_type="event",
                    target_id=event_id,
                    verdict=verdict,
                    cache=cache_hit,
                )
            if verdict.requires_emergency_escalation:
                await store_emergency_escalation(
                    conn,
                    event_id=event_id,
                    normalized_url=url,
                    sha256=video_sha256,
                    verdict=verdict,
                )
            await update_analysis_job(
                conn,
                event_id=event_id,
                url=url,
                media_type="video",
                status="completed",
                image_sha256=video_sha256,
            )
        except Exception as exc:
            await update_analysis_job(conn, event_id=event_id, url=url, media_type="video", status="failed", error=exc)
            raise


async def ensure_consumer_group(redis_client: redis.Redis, stream: str, group: str) -> None:
    try:
        await redis_client.xgroup_create(stream, group, id="0", mkstream=True)
    except ResponseError as exc:
        if "BUSYGROUP" not in str(exc):
            raise


async def move_due_retries(
    redis_client: redis.Redis,
    *,
    stream_maxlen: int,
    now: float | None = None,
    limit: int = 100,
) -> int:
    now = time.time() if now is None else now
    payloads = await redis_client.zrangebyscore(RETRY_QUEUE, 0, now, start=0, num=limit)
    moved = 0
    for payload in payloads:
        removed = await redis_client.zrem(RETRY_QUEUE, payload)
        if removed:
            await xadd_payload(redis_client, JOB_QUEUE, payload, maxlen=stream_maxlen)
            moved += 1
    return moved


async def read_job(redis_client: redis.Redis, *, group: str, consumer: str) -> tuple[str, dict[str, Any]] | None:
    entries = await redis_client.xreadgroup(
        groupname=group,
        consumername=consumer,
        streams={JOB_QUEUE: ">"},
        count=1,
        block=QUEUE_POLL_SECONDS * 1000,
    )
    if not entries:
        return None
    _, messages = entries[0]
    message_id, fields = messages[0]
    return message_id, json.loads(fields["payload"])


async def ack_job(redis_client: redis.Redis, *, group: str, message_id: str) -> None:
    await redis_client.xack(JOB_QUEUE, group, message_id)


async def retry_or_dead_letter(
    redis_client: redis.Redis,
    *,
    group: str,
    message_id: str,
    job: dict[str, Any],
    error: Exception,
    dead_letter_maxlen: int,
) -> None:
    failed_job = with_failure_metadata(job, error)
    if not getattr(error, "retryable", True) or job_attempts(failed_job) >= MAX_JOB_ATTEMPTS:
        await xadd_payload(
            redis_client,
            DEAD_LETTER_QUEUE,
            json.dumps(failed_job),
            maxlen=dead_letter_maxlen,
        )
    else:
        available_at = time.time() + retry_delay_seconds(job_attempts(failed_job))
        await redis_client.zadd(RETRY_QUEUE, {json.dumps(failed_job): available_at})
    await redis_client.xack(JOB_QUEUE, group, message_id)


async def reclaim_stale_jobs(redis_client: redis.Redis, *, group: str, consumer: str) -> list[tuple[str, dict[str, Any]]]:
    try:
        response = await redis_client.execute_command(
            "XAUTOCLAIM",
            JOB_QUEUE,
            group,
            consumer,
            PENDING_IDLE_MS,
            "0-0",
            "COUNT",
            RECOVER_PENDING_COUNT,
        )
    except ResponseError:
        return []
    if len(response) < 2:
        return []
    reclaimed = []
    for message_id, fields in response[1]:
        if "payload" in fields:
            reclaimed.append((message_id, json.loads(fields["payload"])))
    return reclaimed


async def process_stream_job(
    redis_client: redis.Redis,
    conn: asyncpg.Connection,
    model: ModerationModel,
    *,
    group: str,
    message_id: str,
    job: dict[str, Any],
    max_bytes: int,
    timeout_seconds: int,
    max_video_bytes: int,
    max_video_frames: int,
    video_frame_interval_seconds: int,
    dead_letter_maxlen: int,
) -> None:
    image_sha256 = job.get("image_sha256")
    await update_image_job(conn, sha256=image_sha256, status="processing")
    try:
        await process_job(
            conn,
            job,
            model,
            max_bytes,
            timeout_seconds,
            max_video_bytes=max_video_bytes,
            max_video_frames=max_video_frames,
            video_frame_interval_seconds=video_frame_interval_seconds,
        )
    except Exception as exc:
        next_attempts = job_attempts(job) + 1
        retryable = getattr(exc, "retryable", True)
        await update_image_job(
            conn,
            sha256=image_sha256,
            status="failed" if not retryable or next_attempts >= MAX_JOB_ATTEMPTS else "retrying",
            error=exc,
        )
        await retry_or_dead_letter(
            redis_client,
            group=group,
            message_id=message_id,
            job=job,
            error=exc,
            dead_letter_maxlen=dead_letter_maxlen,
        )
    else:
        await update_image_job(conn, sha256=image_sha256, status="completed")
        await ack_job(redis_client, group=group, message_id=message_id)


async def run_worker() -> None:
    redis_url = os.getenv("REDIS_URL", "redis://localhost:6379/0")
    database_url = os.getenv("DATABASE_URL", "postgresql://oracle:oracle@localhost:5432/oracle")
    group = env_str("QUEUE_CONSUMER_GROUP", QUEUE_GROUP)
    consumer = env_str("QUEUE_CONSUMER_NAME", QUEUE_CONSUMER)

    redis_client = redis.from_url(
        redis_url,
        decode_responses=True,
        socket_connect_timeout=10,
        socket_timeout=QUEUE_POLL_SECONDS + 10,
    )
    conn = await asyncpg.connect(database_url)
    await ensure_image_jobs_schema(conn)
    settings = await load_runtime_settings(conn)
    model_signature = provider_signature(settings)
    model = create_moderation_model(settings)
    await ensure_consumer_group(redis_client, JOB_QUEUE, group)

    while True:
        settings = await load_runtime_settings(conn)
        next_model_signature = provider_signature(settings)
        if next_model_signature != model_signature:
            try:
                model = create_moderation_model(settings)
                model_signature = next_model_signature
            except Exception:
                pass

        max_bytes = setting_int(settings, "MAX_IMAGE_BYTES", 10_000_000)
        max_video_bytes = setting_int(settings, "MAX_VIDEO_BYTES", DEFAULT_MAX_VIDEO_BYTES)
        timeout_seconds = setting_int(settings, "IMAGE_FETCH_TIMEOUT_SECONDS", 10)
        max_video_frames = setting_int(settings, "MAX_VIDEO_FRAMES", DEFAULT_MAX_VIDEO_FRAMES)
        video_frame_interval_seconds = setting_int(
            settings,
            "VIDEO_FRAME_INTERVAL_SECONDS",
            DEFAULT_VIDEO_FRAME_INTERVAL_SECONDS,
        )
        stream_maxlen = setting_int(settings, "QUEUE_STREAM_MAXLEN", DEFAULT_STREAM_MAXLEN)
        dead_letter_maxlen = setting_int(settings, "QUEUE_DEAD_LETTER_MAXLEN", DEFAULT_DEAD_LETTER_MAXLEN)

        await move_due_retries(redis_client, stream_maxlen=stream_maxlen)
        stale_jobs = await reclaim_stale_jobs(redis_client, group=group, consumer=consumer)
        for message_id, job in stale_jobs:
            await process_stream_job(
                redis_client,
                conn,
                model,
                group=group,
                message_id=message_id,
                job=job,
                max_bytes=max_bytes,
                timeout_seconds=timeout_seconds,
                max_video_bytes=max_video_bytes,
                max_video_frames=max_video_frames,
                video_frame_interval_seconds=video_frame_interval_seconds,
                dead_letter_maxlen=dead_letter_maxlen,
            )
        try:
            result = await read_job(redis_client, group=group, consumer=consumer)
        except RedisTimeoutError:
            await asyncio.sleep(1)
            continue
        if result is None:
            continue
        message_id, job = result
        await process_stream_job(
            redis_client,
            conn,
            model,
            group=group,
            message_id=message_id,
            job=job,
            max_bytes=max_bytes,
            timeout_seconds=timeout_seconds,
            max_video_bytes=max_video_bytes,
            max_video_frames=max_video_frames,
            video_frame_interval_seconds=video_frame_interval_seconds,
            dead_letter_maxlen=dead_letter_maxlen,
        )


if __name__ == "__main__":
    asyncio.run(run_worker())
