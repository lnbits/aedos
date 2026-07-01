from __future__ import annotations

import base64
from typing import Any

import httpx
from PIL import Image

from moderation import Verdict, csam_suspected_verdict


OPENAI_MODERATIONS_URL = "https://api.openai.com/v1/moderations"


class OpenAIModerationModel:
    def __init__(
        self,
        *,
        api_key: str,
        model: str = "omni-moderation-latest",
        client: httpx.AsyncClient | None = None,
    ) -> None:
        self.api_key = api_key
        self.model = model
        self._client = client

    async def analyse(self, image: Image.Image, payload: bytes, mime_type: str) -> Verdict:
        _ = image
        data_url = image_data_url(payload, mime_type)
        response = await self._post_moderation(data_url)
        return verdict_from_openai_response(response, self.model)

    async def _post_moderation(self, data_url: str) -> dict[str, Any]:
        headers = {
            "authorization": f"Bearer {self.api_key}",
            "content-type": "application/json",
        }
        body = {
            "model": self.model,
            "input": [
                {
                    "type": "image_url",
                    "image_url": {"url": data_url},
                }
            ],
        }
        if self._client is not None:
            response = await self._client.post(OPENAI_MODERATIONS_URL, headers=headers, json=body)
            response.raise_for_status()
            return response.json()

        async with httpx.AsyncClient(timeout=30) as client:
            response = await client.post(OPENAI_MODERATIONS_URL, headers=headers, json=body)
            response.raise_for_status()
            return response.json()


def image_data_url(payload: bytes, mime_type: str) -> str:
    encoded = base64.b64encode(payload).decode("ascii")
    safe_mime_type = mime_type if mime_type.startswith("image/") else "application/octet-stream"
    return f"data:{safe_mime_type};base64,{encoded}"


def verdict_from_openai_response(response: dict[str, Any], model_version: str) -> Verdict:
    result = response.get("results", [{}])[0]
    categories = result.get("categories", {})
    scores = result.get("category_scores", {})
    labels = labels_from_categories(categories)
    confidence = max((float(scores.get(category, 0.0)) for category in categories), default=0.0)

    if "csam-suspected" in labels:
        return csam_suspected_verdict(
            confidence=max(confidence, float(scores.get("sexual/minors", 0.0))),
            source="openai_moderation",
            model_version=model_version,
            explanation="OpenAI moderation returned sexual/minors",
        )

    if labels:
        status = "block" if has_block_label(labels) else "warn"
        return Verdict(
            status=status,
            labels=labels,
            confidence=confidence,
            source="openai_moderation",
            model_version=model_version,
            explanation="OpenAI moderation flagged image categories",
        )

    return Verdict(
        status="safe",
        labels=["safe"],
        confidence=1.0 - confidence,
        source="openai_moderation",
        model_version=model_version,
        explanation="OpenAI moderation did not flag image categories",
    )


def labels_from_categories(categories: dict[str, Any]) -> list[str]:
    labels: list[str] = []
    if categories.get("sexual/minors"):
        labels.append("csam-suspected")
    if categories.get("sexual"):
        labels.extend(["nsfw", "sexual"])
    if categories.get("violence"):
        labels.append("violence")
    if categories.get("violence/graphic"):
        labels.extend(["graphic", "gore"])
    if categories.get("self-harm") or categories.get("self-harm/intent") or categories.get(
        "self-harm/instructions"
    ):
        labels.append("self-harm")
    if categories.get("hate") or categories.get("hate/threatening"):
        labels.append("hate-symbol")
    if categories.get("illicit") or categories.get("illicit/violent"):
        labels.append("scam")
    return dedupe(labels)


def has_block_label(labels: list[str]) -> bool:
    return any(label in {"gore", "self-harm", "hate-symbol"} for label in labels)


def dedupe(labels: list[str]) -> list[str]:
    seen: set[str] = set()
    unique: list[str] = []
    for label in labels:
        if label not in seen:
            seen.add(label)
            unique.append(label)
    return unique
