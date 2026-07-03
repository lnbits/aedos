from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Protocol

from PIL import Image


@dataclass(frozen=True)
class Verdict:
    status: str
    labels: list[str] = field(default_factory=list)
    confidence: float = 0.0
    source: str = "local_model"
    model_version: str = "deterministic-v0"
    explanation: str | None = None
    provider_response: dict[str, Any] | None = None

    @property
    def safe(self) -> bool:
        return self.status == "safe"

    @property
    def warn(self) -> bool:
        return self.status == "warn"

    @property
    def block(self) -> bool:
        return self.status == "block"

    @property
    def requires_emergency_escalation(self) -> bool:
        return "csam-suspected" in self.labels


def csam_suspected_verdict(
    *,
    confidence: float,
    source: str,
    model_version: str,
    explanation: str | None = None,
    provider_response: dict[str, Any] | None = None,
) -> Verdict:
    return Verdict(
        status="block",
        labels=["csam-suspected"],
        confidence=confidence,
        source=source,
        model_version=model_version,
        explanation=explanation or "emergency moderation label requiring operator process",
        provider_response=provider_response,
    )


class ModerationModel(Protocol):
    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        ...


class DeterministicModerationModel:
    """A dependency-free baseline model suitable for local development and tests."""

    model_version = "deterministic-v0"

    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        _ = payload, mime_type
        if image.width < 1 or image.height < 1:
            return Verdict(
                status="error",
                labels=["unknown"],
                confidence=0.0,
                model_version=self.model_version,
                explanation="invalid image dimensions",
            )
        return Verdict(
            status="safe",
            labels=["safe"],
            confidence=0.5,
            model_version=self.model_version,
            explanation="no production model configured",
        )
