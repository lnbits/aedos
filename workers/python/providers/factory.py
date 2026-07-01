from __future__ import annotations

import os

from moderation import DeterministicModerationModel, ModerationModel
from providers.openai_moderation import OpenAIModerationModel


def create_moderation_model() -> ModerationModel:
    provider = os.getenv("MODERATION_PROVIDER", "deterministic").strip().lower()
    if provider == "deterministic":
        return DeterministicModerationModel()
    if provider == "openai":
        api_key = os.getenv("OPENAI_API_KEY", "").strip()
        if not api_key:
            raise RuntimeError("OPENAI_API_KEY is required when MODERATION_PROVIDER=openai")
        return OpenAIModerationModel(
            api_key=api_key,
            model=os.getenv("OPENAI_MODERATION_MODEL", "omni-moderation-latest").strip()
            or "omni-moderation-latest",
        )
    raise RuntimeError(f"unsupported MODERATION_PROVIDER: {provider}")
