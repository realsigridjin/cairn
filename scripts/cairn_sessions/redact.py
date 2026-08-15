"""Redaction pass applied to every string field before it enters a chunk.

Single pure function ``redact_text`` so privacy tests are table-driven.
Rule order follows the architecture brief (st_01a005c1 section 4):
structural drops happen in the scanners (thinking blocks, tool arguments,
attachments are never imported); this module handles the pattern scrub,
the environment-value scrub, and length caps.
"""

from __future__ import annotations

import os
import re

FIRST_PROMPT_CAP = 512
MESSAGE_TEXT_CAP = 4000

# (class, pattern). Order matters: multi-line/private-key first, specific
# token shapes next, JWT before bearer, generic credential assignment last.
PATTERNS: list[tuple[str, "re.Pattern[str]"]] = [
    (
        "private_key",
        re.compile(
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            re.DOTALL,
        ),
    ),
    ("openrouter_key", re.compile(r"sk-or-[A-Za-z0-9-]+")),
    ("anthropic_key", re.compile(r"sk-ant-[A-Za-z0-9-]+")),
    ("aws_key", re.compile(r"AKIA[0-9A-Z]{16}")),
    ("github_token", re.compile(r"(?:ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})")),
    ("slack_token", re.compile(r"xox[baprs]-[A-Za-z0-9-]+")),
    ("jwt", re.compile(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+")),
    ("bearer", re.compile(r"Bearer\s+[A-Za-z0-9._~+/=-]{20,}")),
    (
        "credential",
        re.compile(r"(?i)(?:api[_-]?key|token|secret|password)\s*[:=]\s*[\"']?\S+[\"']?"),
    ),
]

# Environment variables whose *values* must never reach the corpus.
ENV_SCRUB_VARS = ("CAIRN_SERVER_TOKEN", "OPENROUTER_API_KEY")
_MIN_ENV_LEN = 8


def _replacement(cls: str) -> str:
    return f"[REDACTED:{cls}]"


def redact_text(text: str, *, env: dict | None = None) -> tuple[str, list[str]]:
    """Scrub secret patterns from ``text``.

    Returns ``(redacted_text, hits)`` where ``hits`` is the sorted list of
    redaction classes that fired. Pure: same input -> same output.
    """
    if not text:
        return text, []
    hits: set[str] = set()
    out = text
    for cls, pattern in PATTERNS:
        if pattern.search(out):
            hits.add(cls)
            out = pattern.sub(_replacement(cls), out)
    environ = os.environ if env is None else env
    for var in ENV_SCRUB_VARS:
        value = environ.get(var)
        if value and len(value) >= _MIN_ENV_LEN and value in out:
            hits.add("env")
            out = out.replace(value, _replacement("env"))
    return out, sorted(hits)


def cap_text(text: str, cap: int) -> str:
    """Deterministic length cap with an explicit truncation marker."""
    if len(text) <= cap:
        return text
    return text[:cap] + "\n[truncated]"


def redact_first_prompt(text: str | None, *, env: dict | None = None) -> str | None:
    if text is None:
        return None
    redacted, _ = redact_text(text, env=env)
    return cap_text(redacted, FIRST_PROMPT_CAP)


def redact_message_text(text: str, *, env: dict | None = None) -> str:
    redacted, _ = redact_text(text, env=env)
    return cap_text(redacted, MESSAGE_TEXT_CAP)


def redact_title(text: str | None, *, env: dict | None = None) -> str | None:
    if text is None:
        return None
    redacted, _ = redact_text(text, env=env)
    return cap_text(redacted, FIRST_PROMPT_CAP)
