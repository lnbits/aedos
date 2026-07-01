from __future__ import annotations

import hashlib
from dataclasses import dataclass
from io import BytesIO

import imagehash
from PIL import Image


@dataclass(frozen=True)
class ImageFingerprint:
    sha256: str
    phash: str
    mime_type: str
    width: int
    height: int
    bytes: int


def fingerprint_image(payload: bytes, mime_type: str = "application/octet-stream") -> ImageFingerprint:
    image = Image.open(BytesIO(payload))
    image.verify()

    reopened = Image.open(BytesIO(payload))
    width, height = reopened.size
    return ImageFingerprint(
        sha256=hashlib.sha256(payload).hexdigest(),
        phash=str(imagehash.phash(reopened)),
        mime_type=mime_type,
        width=width,
        height=height,
        bytes=len(payload),
    )


def phash_distance(left: str, right: str) -> int:
    return imagehash.hex_to_hash(left) - imagehash.hex_to_hash(right)
