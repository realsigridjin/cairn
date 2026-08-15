"""Shared scanner infrastructure.

Contains:
  * the hard denylist of credential/settings/env paths that must never be
    opened, enforced both at discovery time and at open time;
  * safe IO wrappers every scanner routes through (tests can spy on them);
  * per-store / per-file report structures;
  * the incremental-scan driver implementing the checkpoint protocol:
    unchanged files are skipped, grown append-only JSONL files resume at
    the recorded byte offset, and checkpoint entries are only produced for
    successfully parsed files.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field

from .model import NormalizedSession, SurfaceMessage

# ---------------------------------------------------------------------------
# Denylist: never opened, never read, never stat-ed beyond directory listing.
# ---------------------------------------------------------------------------

DENYLIST_BASENAMES = frozenset(
    {
        "auth.json",
        ".credentials.yaml",
        ".claude.json",
        "settings.json",
        "settings.yaml",
        "models.json",
        "config.yaml",
    }
)


def is_denied(path: str) -> bool:
    name = os.path.basename(path)
    if name in DENYLIST_BASENAMES:
        return True
    if name == ".env" or name.startswith(".env."):
        return True
    return False


class DeniedPathError(PermissionError):
    """Raised when code attempts to open a denylisted path."""


def _guard(path: str) -> None:
    if is_denied(path):
        raise DeniedPathError(f"refusing to open denylisted path: {path}")


def open_text(path: str, **kwargs):
    """Open a text file for reading, after the denylist guard."""
    _guard(path)
    kwargs.setdefault("encoding", "utf-8")
    kwargs.setdefault("errors", "replace")
    return open(path, "r", **kwargs)


def read_bytes(path: str) -> bytes:
    """Read a whole file as bytes, after the denylist guard."""
    _guard(path)
    with open(path, "rb") as fh:
        return fh.read()


def read_json(path: str):
    with open_text(path) as fh:
        return json.load(fh)


def walk_files(root: str):
    """Yield absolute paths under ``root``, skipping denylisted names."""
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not is_denied(d)]
        for name in filenames:
            if is_denied(name):
                continue
            yield os.path.join(dirpath, name)


# ---------------------------------------------------------------------------
# Errors and reports
# ---------------------------------------------------------------------------


class MalformedFileError(Exception):
    """One session file is corrupt/unparseable; isolated to that file."""


class StoreHardError(Exception):
    """A store-level hard stop (e.g. unknown DSH format version).

    Mirrors the harness's own refusal policy: no chunks are emitted from
    that store at all.
    """


@dataclass
class FileError:
    store: str
    path: str
    reason: str

    def to_dict(self) -> dict:
        return {"store": self.store, "path": self.path, "reason": self.reason}


@dataclass
class StoreReport:
    store: str
    root: str
    status: str = "ok"  # ok | absent | error
    files_seen: int = 0
    files_imported: int = 0
    files_skipped_unchanged: int = 0
    errors: list[FileError] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "store": self.store,
            "root": self.root,
            "status": self.status,
            "files_seen": self.files_seen,
            "files_imported": self.files_imported,
            "files_skipped_unchanged": self.files_skipped_unchanged,
            "errors": [e.to_dict() for e in self.errors],
            "warnings": list(self.warnings),
        }


# ---------------------------------------------------------------------------
# Incremental scan driver
# ---------------------------------------------------------------------------


@dataclass
class ParseOutcome:
    """Result of parsing one session file (possibly incrementally).

    ``session.messages`` holds the full surface projection on a fresh parse,
    or ``tail + new`` messages (seq-anchored) on an incremental parse.
    """

    session: NormalizedSession
    end_offset: int
    msg_count: int
    full_windows: int
    tail: list[SurfaceMessage]
    reused_full_windows: int = 0
    extra_ckpt: dict = field(default_factory=dict)


@dataclass
class ParsedFile:
    store: str
    path: str
    mtime_ns: int
    size: int
    session_uid: str
    session: NormalizedSession | None  # None when skipped unchanged
    skipped_unchanged: bool
    reused_full_windows: int
    updated_at_ms: int


def read_jsonl_records(data: bytes, *, start_offset: int, path: str):
    """Parse JSONL bytes into (records, end_offset).

    Mid-stream lines that fail to parse raise ``MalformedFileError``.
    A final line that has no trailing newline and does not parse is treated
    as an in-flight append when resuming (``start_offset > 0``) and simply
    not consumed; on a fresh parse it is malformed (a truncated file).
    """
    records = []
    if not data:
        return records, start_offset
    trailing_record = None
    has_trailing_newline = data.endswith(b"\n")
    raw_lines = data.split(b"\n")
    consumed = len(data)
    if not has_trailing_newline:
        last = raw_lines.pop()
        if last.strip():
            try:
                trailing_record = json.loads(last.decode("utf-8", "replace"))
            except json.JSONDecodeError:
                if start_offset > 0:
                    # In-flight append: hold the partial line for next run.
                    consumed -= len(last)
                else:
                    raise MalformedFileError(
                        f"truncated final JSONL line in {path}"
                    )
    for idx, raw in enumerate(raw_lines):
        if not raw.strip():
            continue
        line_no = idx + 1
        try:
            records.append(json.loads(raw.decode("utf-8", "replace")))
        except json.JSONDecodeError as exc:
            raise MalformedFileError(
                f"malformed JSONL line {line_no} in {path}: {exc.msg}"
            )
    if trailing_record is not None:
        records.append(trailing_record)
    return records, start_offset + consumed


def drive_file(
    *,
    store: str,
    path: str,
    tier: str,
    checkpoint,
    prev_index,
    report: StoreReport,
    parse_fn,
    append_only: bool = True,
) -> ParsedFile | None:
    """Run the checkpoint protocol for one session file.

    ``parse_fn(path, tier, start_offset, prev_state, stat) -> ParseOutcome``.
    Checkpoint entries are only recorded for successfully parsed files, so a
    corrupt file is retried (and reported) on every run until fixed.
    """
    report.files_seen += 1
    try:
        stat = os.stat(path)
    except OSError as exc:
        report.errors.append(FileError(store, path, f"stat failed: {exc}"))
        return None

    entry = checkpoint.entry(path)
    uid = entry.get("session_uid") if entry else None

    if (
        entry
        and uid
        and entry.get("size") == stat.st_size
        and entry.get("mtime_ns") == stat.st_mtime_ns
        and entry.get("tier") == tier
        and entry.get("byte_offset") == stat.st_size
        and prev_index.has_session(uid)
    ):
        report.files_skipped_unchanged += 1
        return ParsedFile(
            store=store,
            path=path,
            mtime_ns=stat.st_mtime_ns,
            size=stat.st_size,
            session_uid=uid,
            session=None,
            skipped_unchanged=True,
            reused_full_windows=0,
            updated_at_ms=int(entry.get("updated_at_ms") or 0),
        )

    start_offset = 0
    prev_state = None
    if (
        append_only
        and entry
        and uid
        and entry.get("tier") == tier
        and prev_index.has_session(uid)
        and stat.st_size >= int(entry.get("byte_offset") or 0)
    ):
        start_offset = int(entry.get("byte_offset") or 0)
        prev_state = entry

    try:
        outcome = parse_fn(
            path, tier=tier, start_offset=start_offset, prev_state=prev_state, stat=stat
        )
    except MalformedFileError as exc:
        report.errors.append(FileError(store, path, str(exc)))
        return None

    session = outcome.session
    ckpt_entry = {
        "size": outcome.end_offset,
        "mtime_ns": stat.st_mtime_ns,
        "byte_offset": outcome.end_offset,
        "session_uid": session.session_uid,
        "tier": tier,
        "msg_count": outcome.msg_count,
        "full_windows": outcome.full_windows,
        "tail": [m.to_dict() for m in outcome.tail],
        "updated_at_ms": session.updated_at_ms,
        "first_prompt": session.first_prompt,
    }
    ckpt_entry.update(outcome.extra_ckpt or {})
    checkpoint.set_entry(path, ckpt_entry)
    report.files_imported += 1
    return ParsedFile(
        store=store,
        path=path,
        mtime_ns=stat.st_mtime_ns,
        size=stat.st_size,
        session_uid=session.session_uid,
        session=session,
        skipped_unchanged=False,
        reused_full_windows=outcome.reused_full_windows,
        updated_at_ms=session.updated_at_ms,
    )


def extract_text_blocks(content) -> str:
    """Concatenate plain-text blocks from a harness content array.

    Thinking blocks, tool-use arguments, attachments and images are
    structural drops: they never enter the corpus.
    """
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    parts: list[str] = []
    if isinstance(content, list):
        for block in content:
            if not isinstance(block, dict):
                continue
            btype = block.get("type")
            if btype in ("text", "input_text", "output_text"):
                text = block.get("text")
                if isinstance(text, str) and text:
                    parts.append(text)
            elif btype == "tool_result":
                inner = extract_text_blocks(block.get("content"))
                if inner:
                    parts.append(inner)
    return "\n".join(parts)
