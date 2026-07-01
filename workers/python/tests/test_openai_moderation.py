from __future__ import annotations

import json

import httpx
from PIL import Image

from providers.openai_moderation import OpenAIModerationModel, image_data_url, labels_from_categories


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
