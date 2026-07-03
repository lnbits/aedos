from __future__ import annotations

import hashlib
import json
from io import BytesIO

import httpx
from PIL import Image

from moderation import Verdict, csam_suspected_verdict
from worker import (
    DEAD_LETTER_QUEUE,
    DEFAULT_DEAD_LETTER_MAXLEN,
    DEFAULT_STREAM_MAXLEN,
    IMAGE_FETCH_HEADERS,
    JOB_QUEUE,
    RETRY_QUEUE,
    VIDEO_FETCH_HEADERS,
    ack_job,
    aggregate_video_verdict,
    error_message,
    fetch_image,
    fetch_video,
    load_runtime_settings,
    move_due_retries,
    process_job,
    provider_signature,
    retry_or_dead_letter,
    store_emergency_escalation,
    update_analysis_job,
)


class RecordingConnection:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []
        self.fetchrow_result: dict[str, object] | None = None
        self.fetch_result: list[dict[str, object]] = []

    async def execute(self, query: str, *args: object) -> None:
        self.calls.append((query, args))

    async def fetchrow(self, query: str, *args: object) -> dict[str, object] | None:
        self.calls.append((query, args))
        return self.fetchrow_result

    async def fetch(self, query: str, *args: object) -> list[dict[str, object]]:
        self.calls.append((query, args))
        return self.fetch_result

    async def fetchval(self, query: str, *args: object) -> str:
        self.calls.append((query, args))
        return "00000000-0000-0000-0000-000000000001"


class FailingModel:
    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        raise AssertionError("model should not be called on image verdict cache hit")


class RecordingModel:
    def __init__(self) -> None:
        self.calls = 0

    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        self.calls += 1
        return Verdict(
            status="warn",
            labels=["violence"],
            confidence=0.77,
            source="test-model",
            model_version="test-v1",
            explanation="fresh ai review",
        )


class NonRetryableError(RuntimeError):
    retryable = False


class RecordingRedis:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []
        self.retry_payloads: list[str] = []

    async def zrangebyscore(
        self,
        key: str,
        minimum: float,
        maximum: float,
        *,
        start: int,
        num: int,
    ) -> list[str]:
        self.calls.append(("zrangebyscore", (key, minimum, maximum, start, num)))
        return self.retry_payloads[start : start + num]

    async def zrem(self, key: str, payload: str) -> int:
        self.calls.append(("zrem", (key, payload)))
        return 1

    async def xadd(self, key: str, fields: dict[str, str], **kwargs: object) -> str:
        self.calls.append(("xadd", (key, fields, kwargs)))
        return "1-0"

    async def zadd(self, key: str, mapping: dict[str, float]) -> int:
        self.calls.append(("zadd", (key, mapping)))
        return len(mapping)

    async def xack(self, key: str, group: str, message_id: str) -> int:
        self.calls.append(("xack", (key, group, message_id)))
        return 1


def png_bytes() -> bytes:
    output = BytesIO()
    Image.new("RGB", (8, 8), "white").save(output, format="PNG")
    return output.getvalue()


def test_error_message_falls_back_to_exception_type_for_blank_errors() -> None:
    assert error_message(TimeoutError()) == "TimeoutError"


async def test_update_analysis_job_inserts_last_error_value() -> None:
    conn = RecordingConnection()

    await update_analysis_job(
        conn,
        event_id="event123",
        url="https://example.com/a.mp4",
        media_type="video",
        image_sha256="abc123",
        status="failed",
        error="decode failed",
    )

    query, args = conn.calls[0]
    assert "insert into analysis_jobs" in query
    assert "$7" in query
    assert len(args) == 7
    assert args[4:] == ("abc123", "failed", "decode failed")
    assert conn.calls[1] == ("select pg_notify('aedos_media', $1)", ("abc123",))


async def test_store_emergency_escalation_records_metadata_only() -> None:
    conn = RecordingConnection()
    verdict = csam_suspected_verdict(
        confidence=0.99,
        source="known_hash_match",
        model_version="hash-v1",
    )

    await store_emergency_escalation(
        conn,
        event_id="event123",
        normalized_url="https://example.com/image.png",
        sha256="a" * 64,
        verdict=verdict,
    )

    query, args = conn.calls[0]
    assert "emergency_escalations" in query
    assert "image_bytes" not in query
    assert "payload" not in query
    assert args[1:] == (
        "event123",
        "a" * 64,
        "https://example.com/image.png",
        "csam-suspected",
        "pending_operator_review",
        0.99,
        "known_hash_match",
        "hash-v1",
        "emergency moderation label requiring operator process",
    )


async def test_process_job_reuses_image_verdict_without_calling_model(monkeypatch) -> None:
    conn = RecordingConnection()
    conn.fetchrow_result = {
        "status": "safe",
        "labels": ["safe"],
        "confidence": 0.88,
        "source": "openai_moderation",
        "model_version": "omni-moderation-latest",
        "explanation": "cached image verdict",
    }

    async def fake_fetch_image(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
        return png_bytes(), "image/png"

    monkeypatch.setattr("worker.fetch_image", fake_fetch_image)

    await process_job(
        conn,
        {"event_id": "event123", "image_urls": ["https://example.com/a.png"]},
        FailingModel(),
        max_bytes=10_000,
        timeout_seconds=10,
    )

    insert_verdict_calls = [
        call
        for call in conn.calls
        if call[0].lstrip().startswith("insert into verdicts")
    ]
    assert any("insert into events" in call[0] for call in conn.calls)
    assert any("insert into event_images" in call[0] for call in conn.calls)
    assert len(insert_verdict_calls) == 1
    _, args = insert_verdict_calls[0]
    assert args[1:4] == ("event", "event123", "safe")
    assert args[12] is True


async def test_process_job_force_recheck_bypasses_image_cache(monkeypatch) -> None:
    conn = RecordingConnection()
    conn.fetchrow_result = {
        "status": "safe",
        "labels": ["safe"],
        "confidence": 0.88,
        "source": "openai_moderation",
        "model_version": "omni-moderation-latest",
        "explanation": "cached image verdict",
    }
    model = RecordingModel()

    async def fake_fetch_image(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
        return png_bytes(), "image/png"

    monkeypatch.setattr("worker.fetch_image", fake_fetch_image)

    await process_job(
        conn,
        {
            "event_id": "admin-recheck",
            "image_urls": ["https://example.com/a.png"],
            "force_recheck": True,
            "image_only": True,
        },
        model,
        max_bytes=10_000,
        timeout_seconds=10,
    )

    insert_verdict_calls = [
        call
        for call in conn.calls
        if call[0].lstrip().startswith("insert into verdicts")
    ]
    assert model.calls == 1
    assert not any("insert into events" in call[0] for call in conn.calls)
    assert not any("insert into event_images" in call[0] for call in conn.calls)
    assert len(insert_verdict_calls) == 1
    _, args = insert_verdict_calls[0]
    assert args[1] == "image"
    assert args[3] == "warn"
    assert args[12] is False


async def test_process_job_reviews_video_frames_and_stores_video_event_verdict(monkeypatch) -> None:
    conn = RecordingConnection()
    model = RecordingModel()

    async def fake_fetch_video(url: str, max_bytes: int, timeout_seconds: int) -> tuple[bytes, str]:
        return b"fake-video", "video/mp4"

    async def fake_extract_video_frames(
        payload: bytes,
        mime_type: str,
        *,
        max_frames: int,
        frame_interval_seconds: int,
    ) -> list[tuple[bytes, str]]:
        assert payload == b"fake-video"
        assert mime_type == "video/mp4"
        assert max_frames == 2
        assert frame_interval_seconds == 3
        return [(png_bytes(), "image/png"), (png_bytes(), "image/png")]

    monkeypatch.setattr("worker.fetch_video", fake_fetch_video)
    monkeypatch.setattr("worker.extract_video_frames", fake_extract_video_frames)

    await process_job(
        conn,
        {"event_id": "event123", "image_urls": [], "video_urls": ["https://example.com/a.mp4"]},
        model,
        max_bytes=10_000,
        timeout_seconds=10,
        max_video_bytes=50_000,
        max_video_frames=2,
        video_frame_interval_seconds=3,
    )

    insert_verdict_calls = [
        call
        for call in conn.calls
        if call[0].lstrip().startswith("insert into verdicts")
    ]
    assert model.calls == 2
    assert any("insert into videos" in call[0] for call in conn.calls)
    assert any("insert into event_videos" in call[0] for call in conn.calls)
    assert len(insert_verdict_calls) == 2
    assert insert_verdict_calls[0][1][1:4] == ("video", hashlib.sha256(b"fake-video").hexdigest(), "warn")
    assert insert_verdict_calls[1][1][1:4] == ("event", "event123", "warn")


def test_aggregate_video_verdict_uses_highest_severity_frame() -> None:
    verdict = aggregate_video_verdict(
        [
            Verdict(status="safe", labels=["safe"], confidence=0.3, source="test", model_version="v1"),
            Verdict(status="block", labels=["csam-suspected"], confidence=0.9, source="test", model_version="v1"),
            Verdict(status="warn", labels=["sexualised"], confidence=0.8, source="test", model_version="v1"),
        ]
    )

    assert verdict.status == "block"
    assert verdict.labels == ["csam-suspected", "sexualised"]
    assert verdict.confidence == 0.9
    assert verdict.provider_response["frame_count"] == 3


async def test_fetch_image_marks_http_403_non_retryable(monkeypatch) -> None:
    async def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(403, request=request)

    real_async_client = httpx.AsyncClient

    def fake_client(*args: object, **kwargs: object) -> httpx.AsyncClient:
        return real_async_client(transport=httpx.MockTransport(handler))

    monkeypatch.setattr("worker.httpx.AsyncClient", fake_client)

    try:
        await fetch_image("https://example.com/blocked.png", max_bytes=10_000, timeout_seconds=10)
    except RuntimeError as exc:
        assert getattr(exc, "retryable", True) is False
        assert "403 Forbidden" in str(exc)
    else:
        raise AssertionError("expected fetch failure")


async def test_fetch_image_sends_browser_image_headers(monkeypatch) -> None:
    seen_headers: dict[str, str] = {}

    async def handler(request: httpx.Request) -> httpx.Response:
        seen_headers["user-agent"] = request.headers["user-agent"]
        seen_headers["accept"] = request.headers["accept"]
        return httpx.Response(200, headers={"content-type": "image/png"}, content=png_bytes(), request=request)

    real_async_client = httpx.AsyncClient

    def fake_client(*args: object, **kwargs: object) -> httpx.AsyncClient:
        headers = kwargs.pop("headers")
        return real_async_client(headers=headers, transport=httpx.MockTransport(handler))

    monkeypatch.setattr("worker.httpx.AsyncClient", fake_client)

    _, mime_type = await fetch_image("https://example.com/image.png", max_bytes=10_000, timeout_seconds=10)

    assert mime_type == "image/png"
    assert seen_headers["user-agent"] == IMAGE_FETCH_HEADERS["User-Agent"]
    assert seen_headers["accept"] == IMAGE_FETCH_HEADERS["Accept"]


async def test_fetch_video_sends_browser_video_headers(monkeypatch) -> None:
    seen_headers: dict[str, str] = {}

    async def handler(request: httpx.Request) -> httpx.Response:
        seen_headers["user-agent"] = request.headers["user-agent"]
        seen_headers["accept"] = request.headers["accept"]
        return httpx.Response(200, headers={"content-type": "video/mp4"}, content=b"fake-video", request=request)

    real_async_client = httpx.AsyncClient

    def fake_client(*args: object, **kwargs: object) -> httpx.AsyncClient:
        headers = kwargs.pop("headers")
        return real_async_client(headers=headers, transport=httpx.MockTransport(handler))

    monkeypatch.setattr("worker.httpx.AsyncClient", fake_client)

    payload, mime_type = await fetch_video("https://example.com/video.mp4", max_bytes=10_000, timeout_seconds=10)

    assert payload == b"fake-video"
    assert mime_type == "video/mp4"
    assert seen_headers["user-agent"] == VIDEO_FETCH_HEADERS["User-Agent"]
    assert seen_headers["accept"] == VIDEO_FETCH_HEADERS["Accept"]


async def test_move_due_retries_requeues_jobs() -> None:
    redis = RecordingRedis()
    redis.retry_payloads = [json.dumps({"event_id": "event123", "image_urls": []})]

    moved = await move_due_retries(redis, stream_maxlen=DEFAULT_STREAM_MAXLEN, now=100)

    assert moved == 1
    assert ("zrangebyscore", (RETRY_QUEUE, 0, 100, 0, 100)) in redis.calls
    assert any(call[0] == "xadd" and call[1][0] == JOB_QUEUE for call in redis.calls)


async def test_retry_or_dead_letter_schedules_retry_before_attempt_limit() -> None:
    redis = RecordingRedis()

    await retry_or_dead_letter(
        redis,
        group="workers",
        message_id="1-0",
        job={"event_id": "event123", "image_urls": [], "attempts": 0},
        error=RuntimeError("provider temporarily unavailable"),
        dead_letter_maxlen=DEFAULT_DEAD_LETTER_MAXLEN,
    )

    assert any(call[0] == "zadd" and call[1][0] == RETRY_QUEUE for call in redis.calls)
    assert ("xack", (JOB_QUEUE, "workers", "1-0")) in redis.calls


async def test_retry_or_dead_letter_writes_dead_letter_after_attempt_limit() -> None:
    redis = RecordingRedis()

    await retry_or_dead_letter(
        redis,
        group="workers",
        message_id="1-0",
        job={"event_id": "event123", "image_urls": [], "attempts": 4},
        error=RuntimeError("permanent failure"),
        dead_letter_maxlen=DEFAULT_DEAD_LETTER_MAXLEN,
    )

    assert any(call[0] == "xadd" and call[1][0] == DEAD_LETTER_QUEUE for call in redis.calls)
    assert ("xack", (JOB_QUEUE, "workers", "1-0")) in redis.calls


async def test_retry_or_dead_letter_writes_dead_letter_for_non_retryable_error() -> None:
    redis = RecordingRedis()

    await retry_or_dead_letter(
        redis,
        group="workers",
        message_id="1-0",
        job={"event_id": "event123", "image_urls": [], "attempts": 0},
        error=NonRetryableError("project is not allowed to process this request"),
        dead_letter_maxlen=DEFAULT_DEAD_LETTER_MAXLEN,
    )

    assert any(call[0] == "xadd" and call[1][0] == DEAD_LETTER_QUEUE for call in redis.calls)
    assert not any(call[0] == "zadd" and call[1][0] == RETRY_QUEUE for call in redis.calls)
    assert ("xack", (JOB_QUEUE, "workers", "1-0")) in redis.calls


async def test_ack_job_uses_configured_group() -> None:
    redis = RecordingRedis()

    await ack_job(redis, group="custom-workers", message_id="1-0")

    assert ("xack", (JOB_QUEUE, "custom-workers", "1-0")) in redis.calls


async def test_load_runtime_settings_reads_admin_settings() -> None:
    conn = RecordingConnection()
    conn.fetch_result = [
        {"key": "MODERATION_PROVIDER", "value": "openai"},
        {"key": "OPENAI_MODERATION_MODEL", "value": "omni-moderation-latest"},
    ]

    settings = await load_runtime_settings(conn)

    assert settings["MODERATION_PROVIDER"] == "openai"
    assert settings["OPENAI_MODERATION_MODEL"] == "omni-moderation-latest"


def test_provider_signature_uses_dashboard_settings(monkeypatch) -> None:
    monkeypatch.setenv("MODERATION_PROVIDER", "deterministic")

    signature = provider_signature(
        {
            "MODERATION_PROVIDER": "openai",
            "OPENAI_API_KEY": "sk-test",
            "OPENAI_MODERATION_MODEL": "model-a",
        }
    )

    assert signature == ("openai", "sk-test", "model-a")
