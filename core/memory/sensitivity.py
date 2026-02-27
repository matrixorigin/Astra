"""Sensitivity filter — block PII and credentials from long-term memory."""

from __future__ import annotations

import re
from dataclasses import dataclass

# Patterns that should never be persisted into memories.
_PATTERNS: list[tuple[str, re.Pattern]] = [
    ("email", re.compile(r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z]{2,}")),
    ("phone", re.compile(r"\b\d{3}[-.]?\d{3,4}[-.]?\d{4}\b")),
    ("ssn", re.compile(r"\b\d{3}-\d{2}-\d{4}\b")),
    ("credit_card", re.compile(r"\b(?:\d[ -]*?){13,19}\b")),
    ("aws_key", re.compile(r"(?:AKIA|ABIA|ACCA|ASIA)[0-9A-Z]{16}")),
    ("private_key", re.compile(r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----")),
    ("bearer_token", re.compile(r"Bearer\s+[A-Za-z0-9\-._~+/]+=*", re.IGNORECASE)),
    ("password_assignment", re.compile(r"(?:password|passwd|secret)\s*[:=]\s*\S+", re.IGNORECASE)),
]


@dataclass
class SensitivityResult:
    blocked: bool
    matched_labels: list[str]


def check_sensitivity(text: str) -> SensitivityResult:
    """Return which sensitivity patterns matched. Empty = safe to persist."""
    matched = [label for label, pat in _PATTERNS if pat.search(text)]
    return SensitivityResult(blocked=bool(matched), matched_labels=matched)
