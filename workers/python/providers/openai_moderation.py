from __future__ import annotations

import base64
from typing import Any

import httpx
from PIL import Image

from moderation import Verdict, csam_suspected_verdict


OPENAI_MODERATIONS_URL = "https://api.openai.com/v1/moderations"
SEXUALISED_SCORE_THRESHOLD = 0.30


class OpenAIProviderError(RuntimeError):
    def __init__(self, message: str, *, retryable: bool) -> None:
        super().__init__(message)
        self.retryable = retryable


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
            raise_for_openai_status(response)
            return response.json()

        async with httpx.AsyncClient(timeout=30) as client:
            response = await client.post(OPENAI_MODERATIONS_URL, headers=headers, json=body)
            raise_for_openai_status(response)
            return response.json()


def image_data_url(payload: bytes, mime_type: str) -> str:
    encoded = base64.b64encode(payload).decode("ascii")
    safe_mime_type = mime_type if mime_type.startswith("image/") else "application/octet-stream"
    return f"data:{safe_mime_type};base64,{encoded}"


def raise_for_openai_status(response: httpx.Response) -> None:
    if response.is_success:
        return
    retry_after = response.headers.get("retry-after")
    message = ""
    try:
        body = response.json()
    except ValueError:
        body = response.text
    if isinstance(body, dict):
        error = body.get("error")
        if isinstance(error, dict):
            message = str(error.get("message") or error.get("code") or error.get("type") or "")
        else:
            message = str(body)
    else:
        message = body
    details = f"OpenAI moderation returned HTTP {response.status_code}"
    if message:
        details = f"{details}: {message}"
    if retry_after:
        details = f"{details} Retry after {retry_after} seconds."
    retryable = response.status_code >= 500 or (response.status_code == 429 and retry_after is not None)
    raise OpenAIProviderError(details, retryable=retryable)


def verdict_from_openai_response(response: dict[str, Any], model_version: str) -> Verdict:
    result = response.get("results", [{}])[0]
    categories = result.get("categories", {})
    scores = result.get("category_scores", {})
    provider_response = openai_provider_response(response)
    labels = labels_from_categories(categories)
    confidence = max((float(scores.get(category, 0.0)) for category in categories), default=0.0)
    sexual_score = float(scores.get("sexual", 0.0))

    if "csam-suspected" in labels:
        return csam_suspected_verdict(
            confidence=max(confidence, float(scores.get("sexual/minors", 0.0))),
            source="openai_moderation",
            model_version=model_version,
            explanation="OpenAI moderation returned sexual/minors",
            provider_response=provider_response,
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
            provider_response=provider_response,
        )

    if sexual_score >= SEXUALISED_SCORE_THRESHOLD:
        return Verdict(
            status="warn",
            labels=["sexualised"],
            confidence=sexual_score,
            source="openai_moderation",
            model_version=model_version,
            explanation=f"OpenAI sexual score {sexual_score:.3f} met sexualised threshold {SEXUALISED_SCORE_THRESHOLD:.2f}",
            provider_response=provider_response,
        )

    return Verdict(
        status="safe",
        labels=["safe"],
        confidence=1.0 - confidence,
        source="openai_moderation",
        model_version=model_version,
        explanation="OpenAI moderation did not flag image categories",
        provider_response=provider_response,
    )


def openai_provider_response(response: dict[str, Any]) -> dict[str, Any]:
    result = response.get("results", [{}])[0]
    return {
        "id": response.get("id"),
        "model": response.get("model"),
        "flagged": result.get("flagged"),
        "categories": result.get("categories", {}),
        "category_scores": result.get("category_scores", {}),
        "category_applied_input_types": result.get("category_applied_input_types", {}),
    }


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
