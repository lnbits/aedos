from __future__ import annotations

from io import BytesIO

from PIL import Image

from hashing import fingerprint_image, phash_distance


def png_bytes(color: str) -> bytes:
    output = BytesIO()
    Image.new("RGB", (16, 16), color=color).save(output, format="PNG")
    return output.getvalue()


def test_fingerprint_is_stable_for_identical_images() -> None:
    payload = png_bytes("red")
    first = fingerprint_image(payload, "image/png")
    second = fingerprint_image(payload, "image/png")

    assert first.sha256 == second.sha256
    assert first.phash == second.phash
    assert first.width == 16
    assert first.height == 16


def test_phash_distance_identifies_near_duplicate_flat_images() -> None:
    red = fingerprint_image(png_bytes("red"), "image/png")
    slightly_red = fingerprint_image(png_bytes((254, 0, 0)), "image/png")

    assert phash_distance(red.phash, slightly_red.phash) <= 2
