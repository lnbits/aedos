from __future__ import annotations

from moderation import csam_suspected_verdict
from worker import store_emergency_escalation


class RecordingConnection:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []

    async def execute(self, query: str, *args: object) -> None:
        self.calls.append((query, args))


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
