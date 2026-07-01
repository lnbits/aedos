from __future__ import annotations

import os
from collections.abc import Mapping

from moderation import DeterministicModerationModel, ModerationModel
from providers.openai_moderation import OpenAIModerationModel


def setting(settings: Mapping[str, str] | None, key: str, default: str = "") -> str:
    if settings and key in settings:
        return settings[key]
    return os.getenv(key, default)


def create_moderation_model(settings: Mapping[str, str] | None = None) -> ModerationModel:
    provider = setting(settings, "MODERATION_PROVIDER", "deterministic").strip().lower()
    if provider == "deterministic":
        return DeterministicModerationModel()
    if provider == "openai":
        api_key = setting(settings, "OPENAI_API_KEY").strip()
        if not api_key:
            raise RuntimeError("OPENAI_API_KEY is required when MODERATION_PROVIDER=openai")
        return OpenAIModerationModel(
            api_key=api_key,
            model=setting(settings, "OPENAI_MODERATION_MODEL", "omni-moderation-latest").strip()
            or "omni-moderation-latest",
        )
    raise RuntimeError(f"unsupported MODERATION_PROVIDER: {provider}")
