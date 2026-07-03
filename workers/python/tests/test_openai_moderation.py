from __future__ import annotations

import json

import httpx
from PIL import Image

from providers.openai_moderation import (
    OpenAIModerationModel,
    OpenAIProviderError,
    image_data_url,
    labels_from_categories,
    openai_provider_response,
    raise_for_openai_status,
)


def test_image_data_url_encodes_exact_payload() -> None:
    assert image_data_url(b"abc", "image/png") == "data:image/png;base64,YWJj"


def test_labels_from_categories_maps_openai_categories() -> None:
    labels = labels_from_categories(
        {
            "sexual": True,
            "violence": True,
            "violence/graphic": True,
            "self-harm": False,
            "sexual/minors": False,
        }
    )

    assert labels == ["nsfw", "sexual", "violence", "graphic", "gore"]


def test_openai_provider_response_stores_compact_audit_shape() -> None:
    compact = openai_provider_response(
        {
            "id": "modr_123",
            "model": "omni-moderation-latest",
            "created": 123456,
            "large_unused_field": "x" * 1000,
            "results": [
                {
                    "flagged": False,
                    "categories": {"sexual": False},
                    "category_scores": {"sexual": 0.18},
                    "category_applied_input_types": {"sexual": ["image"]},
                    "extra_result_data": {"debug": True},
                }
            ],
        }
    )

    assert compact == {
        "id": "modr_123",
        "model": "omni-moderation-latest",
        "flagged": False,
        "categories": {"sexual": False},
        "category_scores": {"sexual": 0.18},
        "category_applied_input_types": {"sexual": ["image"]},
    }


async def test_openai_moderation_model_returns_safe_verdict() -> None:
    async def handler(request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        assert body["model"] == "omni-moderation-latest"
        assert body["input"][0]["image_url"]["url"].startswith("data:image/png;base64,")
        return httpx.Response(
            200,
            json={
                "results": [
                    {
                        "flagged": False,
                        "categories": {"sexual": False, "violence": False},
                        "category_scores": {"sexual": 0.01, "violence": 0.02},
                    }
                ]
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        model = OpenAIModerationModel(api_key="test-key", client=client)
        verdict = await model.analyse(Image.new("RGB", (8, 8), "white"), b"abc", "image/png")

    assert verdict.status == "safe"
    assert verdict.labels == ["safe"]
    assert verdict.source == "openai_moderation"
    assert verdict.provider_response == {
        "id": None,
        "model": None,
        "flagged": False,
        "categories": {"sexual": False, "violence": False},
        "category_scores": {"sexual": 0.01, "violence": 0.02},
        "category_applied_input_types": {},
    }


async def test_openai_moderation_model_warns_for_sexualised_score() -> None:
    async def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={
                "results": [
                    {
                        "flagged": False,
                        "categories": {"sexual": False, "sexual/minors": False},
                        "category_scores": {"sexual": 0.42, "sexual/minors": 0.0},
                    }
                ]
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        model = OpenAIModerationModel(api_key="test-key", client=client)
        verdict = await model.analyse(Image.new("RGB", (8, 8), "white"), b"abc", "image/png")

    assert verdict.status == "warn"
    assert verdict.labels == ["sexualised"]
    assert verdict.confidence == 0.42
    assert "sexual score 0.420" in (verdict.explanation or "")
    assert verdict.provider_response
    assert verdict.provider_response["category_scores"]["sexual"] == 0.42


async def test_openai_moderation_model_escalates_sexual_minors() -> None:
    async def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={
                "results": [
                    {
                        "flagged": True,
                        "categories": {"sexual/minors": True, "sexual": True},
                        "category_scores": {"sexual/minors": 0.97, "sexual": 0.99},
                    }
                ]
            },
        )

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        model = OpenAIModerationModel(api_key="test-key", client=client)
        verdict = await model.analyse(Image.new("RGB", (8, 8), "white"), b"abc", "image/png")

    assert verdict.status == "block"
    assert verdict.labels == ["csam-suspected"]
    assert verdict.requires_emergency_escalation


def test_openai_error_includes_response_message_and_retry_after() -> None:
    response = httpx.Response(
        429,
        headers={"retry-after": "12"},
        json={"error": {"message": "You exceeded your current quota."}},
    )

    try:
        raise_for_openai_status(response)
    except OpenAIProviderError as exc:
        assert "HTTP 429" in str(exc)
        assert "You exceeded your current quota" in str(exc)
        assert "Retry after 12 seconds" in str(exc)
        assert exc.retryable is True
    else:
        raise AssertionError("expected OpenAIProviderError")


def test_openai_429_without_retry_after_is_not_retryable() -> None:
    response = httpx.Response(
        429,
        json={"error": {"message": "Too Many Requests", "type": "invalid_request_error"}},
    )

    try:
        raise_for_openai_status(response)
    except OpenAIProviderError as exc:
        assert "HTTP 429" in str(exc)
        assert "Too Many Requests" in str(exc)
        assert exc.retryable is False
    else:
        raise AssertionError("expected OpenAIProviderError")
