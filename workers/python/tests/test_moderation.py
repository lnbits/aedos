from __future__ import annotations

from PIL import Image

from moderation import DeterministicModerationModel, csam_suspected_verdict


async def test_default_model_is_swappable_safe_baseline() -> None:
    verdict = await DeterministicModerationModel().analyse(
        Image.new("RGB", (8, 8), "white"),
        b"payload",
        "image/png",
    )

    assert verdict.status == "safe"
    assert verdict.labels == ["safe"]
    assert verdict.model_version == "deterministic-v0"


def test_csam_suspected_verdict_is_emergency_block() -> None:
    verdict = csam_suspected_verdict(
        confidence=0.98,
        source="known_hash_match",
        model_version="hash-v1",
    )

    assert verdict.status == "block"
    assert verdict.block
    assert verdict.labels == ["csam-suspected"]
    assert verdict.requires_emergency_escalation
