from __future__ import annotations

from io import BytesIO

from PIL import Image

from moderation import Verdict, csam_suspected_verdict
from worker import process_job, store_emergency_escalation


class RecordingConnection:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []
        self.fetchrow_result: dict[str, object] | None = None

    async def execute(self, query: str, *args: object) -> None:
        self.calls.append((query, args))

    async def fetchrow(self, query: str, *args: object) -> dict[str, object] | None:
        self.calls.append((query, args))
        return self.fetchrow_result


class FailingModel:
    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        raise AssertionError("model should not be called on image verdict cache hit")


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
    assert len(insert_verdict_calls) == 1
    _, args = insert_verdict_calls[0]
    assert args[1:4] == ("event", "event123", "safe")
    assert args[12] is True
