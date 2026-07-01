from __future__ import annotations

import json
from io import BytesIO

from PIL import Image

from moderation import Verdict, csam_suspected_verdict
from worker import (
    DEAD_LETTER_QUEUE,
    DEFAULT_DEAD_LETTER_MAXLEN,
    DEFAULT_STREAM_MAXLEN,
    JOB_QUEUE,
    RETRY_QUEUE,
    ack_job,
    load_runtime_settings,
    move_due_retries,
    process_job,
    provider_signature,
    retry_or_dead_letter,
    store_emergency_escalation,
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
