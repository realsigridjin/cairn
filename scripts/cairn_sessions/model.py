"""Normalized session schema shared by all store scanners.

One ``NormalizedSession`` per discovered session, regardless of harness.
Field contract follows the architecture brief (st_01a005c1 section 2).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone

SOURCES = ("dsh", "senpi", "pi", "codex", "claude-code")
SURFACE_ROLES = ("user", "assistant", "tool")

STORE_FORMATS = ("jsonl", "jsonl.zstd", "sqlite", "json")

WINDOW = 8  # surface messages per emitted chunk window (fixed boundaries)

TIER_METADATA = "metadata"
TIER_TRANSCRIPT = "transcript"
TIERS = (TIER_METADATA, TIER_TRANSCRIPT)


def iso_to_ms(value) -> int | None:
    """Best-effort conversion of an ISO-8601 string (or epoch ms int) to ms."""
    if value is None:
        return None
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return int(value)
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(text)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1000)


def ms_to_iso(ms: int | None) -> str | None:
    """Deterministic ISO-8601 rendering of epoch milliseconds (UTC)."""
    if ms is None:
        return None
    dt = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{int(ms) % 1000:03d}Z"


def make_session_uid(source: str, native_id: str) -> str:
    """Deterministic identity: ``{source}:{native_uuid}``."""
    return f"{source}:{native_id}"


@dataclass
class SurfaceMessage:
    """One redacted surface-projection message (user/assistant/tool)."""

    seq: int
    role: str
    ts_ms: int | None
    text: str

    def to_dict(self) -> dict:
        return {"seq": self.seq, "role": self.role, "ts_ms": self.ts_ms, "text": self.text}

    @classmethod
    def from_dict(cls, d: dict) -> "SurfaceMessage":
        return cls(
            seq=int(d["seq"]),
            role=str(d["role"]),
            ts_ms=d.get("ts_ms"),
            text=str(d.get("text", "")),
        )


@dataclass
class Usage:
    input: int | None = None
    output: int | None = None
    cache_read: int | None = None
    cache_write: int | None = None
    total: int | None = None
    cost: float | None = None

    def is_empty(self) -> bool:
        return all(
            getattr(self, k) is None
            for k in ("input", "output", "cache_read", "cache_write", "total", "cost")
        )

    def to_dict(self) -> dict:
        return {
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "total": self.total,
            "cost": self.cost,
        }


@dataclass
class NormalizedSession:
    session_uid: str
    source: str  # one of SOURCES
    native_id: str
    root_session_id: str
    parent_id: str | None = None
    depth: int = 0
    agent_role: str | None = None
    cwd: str = ""
    repo_path: str | None = None
    git_branch: str | None = None
    provider: str | None = None
    model: str | None = None
    title: str | None = None
    first_prompt: str | None = None
    created_at_ms: int = 0
    updated_at_ms: int = 0
    message_count: int = 0
    usage: Usage | None = None
    subagent_count: int = 0
    store_path: str = ""
    store_format: str = "jsonl"
    messages: list[SurfaceMessage] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "session_uid": self.session_uid,
            "source": self.source,
            "native_id": self.native_id,
            "root_session_id": self.root_session_id,
            "parent_id": self.parent_id,
            "depth": self.depth,
            "agent_role": self.agent_role,
            "cwd": self.cwd,
            "repo_path": self.repo_path,
            "git_branch": self.git_branch,
            "provider": self.provider,
            "model": self.model,
            "title": self.title,
            "first_prompt": self.first_prompt,
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
            "message_count": self.message_count,
            "usage": self.usage.to_dict() if self.usage else None,
            "subagent_count": self.subagent_count,
            "store_path": self.store_path,
            "store_format": self.store_format,
            "messages": [m.to_dict() for m in self.messages],
        }
