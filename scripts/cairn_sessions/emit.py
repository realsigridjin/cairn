"""Canonical chunk emission.

Maps normalized sessions to ``ChunkInput`` records for ``cairn ingest``
and renders the canonical corpus JSONL deterministically:

  * chunk ids are immutable and content-derived:
    ``s:{session_uid}:meta`` and ``s:{session_uid}:m:{start}-{end}`` with
    fixed 8-message window boundaries, so an append-only session grows by
    minting only *new* window ids while full windows re-emit byte-identical;
  * the corpus is a dict keyed by chunk id; re-import of unchanged files
    reuses previous corpus lines verbatim; everything else is a pure
    function of file contents, so reruns are byte-identical;
  * embedding vectors from a previous ``cairn embed`` pass are carried
    forward onto unchanged chunks so snapshot re-embeds only new/changed
    chunks.
"""

from __future__ import annotations

import json
import os

from . import IMPORTER_VERSION
from .model import (
    TIER_METADATA,
    TIER_TRANSCRIPT,
    TIERS,
    WINDOW,
    NormalizedSession,
    SurfaceMessage,
    ms_to_iso,
)


# ---------------------------------------------------------------------------
# Previous-corpus index
# ---------------------------------------------------------------------------


class PrevCorpusIndex:
    """Index over a previously written canonical JSONL file."""

    def __init__(self):
        self.lines: dict[str, str] = {}  # chunk id -> raw JSONL line
        self.chunks: dict[str, dict] = {}  # chunk id -> parsed chunk

    @classmethod
    def load(cls, path: str | None) -> "PrevCorpusIndex":
        idx = cls()
        if not path or not os.path.exists(path):
            return idx
        try:
            with open(path, "r", encoding="utf-8") as fh:
                for raw in fh:
                    line = raw.rstrip("\n")
                    if not line.strip():
                        continue
                    try:
                        chunk = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    cid = chunk.get("id")
                    if isinstance(cid, str):
                        idx.lines[cid] = line
                        idx.chunks[cid] = chunk
        except OSError:
            pass
        return idx

    def has_session(self, session_uid: str) -> bool:
        prefix = f"s:{session_uid}:"
        return any(cid.startswith(prefix) for cid in self.lines)

    def by_session(self, session_uid: str) -> dict[str, str]:
        prefix = f"s:{session_uid}:"
        return {cid: raw for cid, raw in self.lines.items() if cid.startswith(prefix)}

    def windows_before(self, session_uid: str, start_boundary: int) -> dict[str, str]:
        """Window chunk lines whose window start is below ``start_boundary``."""
        prefix = f"s:{session_uid}:m:"
        out = {}
        for cid, raw in self.lines.items():
            if not cid.startswith(prefix):
                continue
            range_part = cid[len(prefix):]
            try:
                start = int(range_part.split("-", 1)[0])
            except ValueError:
                continue
            if start < start_boundary:
                out[cid] = raw
        return out


# ---------------------------------------------------------------------------
# Chunk construction
# ---------------------------------------------------------------------------


def _chunk_metadata(session: NormalizedSession, doc_type: str, batch_iso: str | None,
                    extra: dict | None = None) -> dict:
    usage = session.usage.to_dict() if session.usage else None
    meta = {
        "doc_type": doc_type,
        "source": session.source,
        "session_uid": session.session_uid,
        "native_id": session.native_id,
        "root_session_id": session.root_session_id,
        "parent_id": session.parent_id,
        "depth": session.depth,
        "agent_role": session.agent_role,
        "cwd": session.cwd,
        "repo_path": session.repo_path,
        "git_branch": session.git_branch,
        "provider": session.provider,
        "model": session.model,
        "store_path": session.store_path,
        "store_format": session.store_format,
        "created_at_ms": session.created_at_ms,
        "updated_at_ms": session.updated_at_ms,
        "message_count": session.message_count,
        "usage_total": usage.get("total") if usage else None,
        "usage_cost": usage.get("cost") if usage else None,
        "subagent_count": session.subagent_count,
        "import_batch": batch_iso,
        "importer_version": IMPORTER_VERSION,
    }
    if extra:
        meta.update(extra)
    return meta


def chunk_meta(session: NormalizedSession, batch_iso: str | None) -> dict:
    lines = [f"session: {session.title or session.native_id}"]
    if session.first_prompt:
        lines.append(f"first_prompt: {session.first_prompt}")
    lines.append(f"source: {session.source}")
    if session.model or session.provider:
        lines.append(
            "model: " + "/".join(p for p in (session.provider, session.model) if p)
        )
    lines.append(f"cwd: {session.cwd}")
    if session.repo_path:
        lines.append(f"repo: {session.repo_path}")
    if session.git_branch:
        lines.append(f"git_branch: {session.git_branch}")
    lines.append(f"messages: {session.message_count}")
    if session.usage and not session.usage.is_empty():
        lines.append(
            f"usage: total={session.usage.total} cost={session.usage.cost}"
        )
    lines.append(
        f"created: {ms_to_iso(session.created_at_ms)} "
        f"updated: {ms_to_iso(session.updated_at_ms)}"
    )
    lines.append(f"store: {session.store_path}")
    return {
        "id": f"s:{session.session_uid}:meta",
        "text": "\n".join(lines),
        "metadata": _chunk_metadata(session, "session_meta", batch_iso),
    }


def chunk_window(session: NormalizedSession, window_index: int,
                 messages: list[SurfaceMessage], batch_iso: str | None) -> dict:
    start = window_index * WINDOW
    end = start + WINDOW - 1
    text = "\n".join(f"[{m.role}] {m.text}" for m in messages)
    return {
        "id": f"s:{session.session_uid}:m:{start}-{end}",
        "text": text,
        "metadata": _chunk_metadata(
            session,
            "session_messages",
            batch_iso,
            {"seq_start": start, "seq_end": end},
        ),
    }


def group_windows(messages: list[SurfaceMessage]):
    """Yield ``(window_index, [messages])`` over fixed 8-message boundaries."""
    buckets: dict[int, list[SurfaceMessage]] = {}
    for msg in messages:
        buckets.setdefault(msg.seq // WINDOW, []).append(msg)
    for index in sorted(buckets):
        yield index, buckets[index]


def _dump_with_carry(chunk: dict, prev: PrevCorpusIndex) -> str:
    """Serialize a chunk, carrying a previous embedding vector forward when
    the chunk's text+metadata are unchanged (embedding cache)."""
    old = prev.chunks.get(chunk["id"])
    if old is not None and "vector" in old:
        if old.get("text") == chunk["text"] and old.get("metadata") == chunk["metadata"]:
            chunk = dict(chunk)
            chunk["vector"] = old["vector"]
    return json.dumps(chunk, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


# ---------------------------------------------------------------------------
# Corpus assembly
# ---------------------------------------------------------------------------


def build_corpus(parsed_files, prev: PrevCorpusIndex, warnings: list[str]) -> dict[str, str]:
    """Build the canonical corpus (chunk id -> JSONL line) deterministically.

    Duplicate ``session_uid`` collisions resolve last-mtime-wins and are
    logged as warnings.
    """
    by_uid: dict[str, object] = {}
    for pf in parsed_files:
        current = by_uid.get(pf.session_uid)
        if current is None:
            by_uid[pf.session_uid] = pf
            continue
        winner, loser = (pf, current) if pf.mtime_ns >= current.mtime_ns else (current, pf)
        warnings.append(
            "duplicate session_uid "
            f"{pf.session_uid}: keeping {winner.path} (mtime_ns={winner.mtime_ns}), "
            f"ignoring {loser.path}"
        )
        by_uid[pf.session_uid] = winner

    batch_ms = max((pf.updated_at_ms for pf in by_uid.values()), default=0)
    batch_iso = ms_to_iso(batch_ms) if batch_ms else None

    lines: dict[str, str] = {}
    for uid in sorted(by_uid):
        pf = by_uid[uid]
        if pf.skipped_unchanged:
            lines.update(prev.by_session(uid))
            continue
        session = pf.session
        meta = chunk_meta(session, batch_iso)
        lines[meta["id"]] = _dump_with_carry(meta, prev)
        if pf.reused_full_windows:
            lines.update(prev.windows_before(uid, pf.reused_full_windows * WINDOW))
        for index, window_messages in group_windows(session.messages):
            chunk = chunk_window(session, index, window_messages, batch_iso)
            lines[chunk["id"]] = _dump_with_carry(chunk, prev)
    return lines


def write_corpus(lines: dict[str, str], out_path: str) -> int:
    """Write the canonical corpus atomically; returns the chunk count."""
    directory = os.path.dirname(os.path.abspath(out_path))
    os.makedirs(directory, exist_ok=True)
    tmp = os.path.join(directory, f".canonical.{os.getpid()}.tmp")
    with open(tmp, "w", encoding="utf-8") as fh:
        for cid in sorted(lines):
            fh.write(lines[cid])
            fh.write("\n")
    os.replace(tmp, out_path)
    return len(lines)
