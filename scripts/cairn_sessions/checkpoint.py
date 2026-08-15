"""Incremental-import checkpoint.

Stored at ``~/.cairn/session-import/checkpoint.json`` (overridable).
Written atomically (``os.replace``) and only *after* the canonical corpus
write succeeds, so a crashed run re-imports rather than skips.

Entry shape per store file (absolute path key)::

    {"size": 384095, "mtime_ns": 1765808298000000000,
     "byte_offset": 384095, "session_uid": "senpi:01a005b7-...",
     "tier": "transcript", "msg_count": 21, "full_windows": 2,
     "tail": [{"seq": 16, "role": "user", "ts_ms": ..., "text": "..."}, ...],
     "updated_at_ms": 1765808400000, "first_prompt": "..."}
"""

from __future__ import annotations

import json
import os

CHECKPOINT_VERSION = 1


class Checkpoint:
    def __init__(self, path: str, data: dict | None = None):
        self.path = path
        self.data = data or {"version": CHECKPOINT_VERSION, "stores": {}}
        if "stores" not in self.data or not isinstance(self.data["stores"], dict):
            self.data["stores"] = {}

    @classmethod
    def load(cls, path: str) -> "Checkpoint":
        try:
            with open(path, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError):
            data = None
        if not isinstance(data, dict) or data.get("version") != CHECKPOINT_VERSION:
            data = None
        return cls(path, data)

    def entry(self, path: str) -> dict | None:
        return self.data["stores"].get(os.path.abspath(path))

    def set_entry(self, path: str, entry: dict) -> None:
        self.data["stores"][os.path.abspath(path)] = entry

    def save(self) -> None:
        directory = os.path.dirname(os.path.abspath(self.path))
        os.makedirs(directory, exist_ok=True)
        tmp = os.path.join(directory, f".checkpoint.{os.getpid()}.tmp")
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(self.data, fh, ensure_ascii=False, sort_keys=True, indent=2)
            fh.write("\n")
        os.replace(tmp, self.path)
