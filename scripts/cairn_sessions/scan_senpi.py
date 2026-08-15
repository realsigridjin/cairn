"""Senpi / pi session store scanner.

Store layout (per st_01a005ba):
    ~/.senpi/agent/sessions/--<encoded-cwd>--/<ISO-ts>_<uuid>.jsonl
    ~/.pi/agent/sessions/--<encoded-cwd>--/<ISO-ts>_<uuid>.jsonl

Event-DAG JSONL: first line is the ``session`` header (format version 3),
followed by events (``model_change``, ``message``, ...). Lineage for omo
senpi-task children comes from ``<repo>/.omo/senpi-task/tasks/st_*.json``
(parent_session_id / root_session_id / depth) matched via the
``children/<task_id>/`` path component of the session file.

pi and senpi share the format; the source label is derived from the store
path (a ``.pi`` component means pi).
"""

from __future__ import annotations

import functools
import json
import os

from . import redact, scanutil
from .model import (
    TIER_TRANSCRIPT,
    WINDOW,
    NormalizedSession,
    SurfaceMessage,
    Usage,
    iso_to_ms,
    make_session_uid,
)

SENPI_FORMAT_VERSIONS = frozenset({3})

ROLE_MAP = {"user": "user", "assistant": "assistant", "toolResult": "tool", "tool": "tool"}


def _source_for(path: str) -> str:
    parts = set(path.split(os.sep))
    return "pi" if ".pi" in parts else "senpi"


def _normalize_usage(raw) -> Usage | None:
    if not isinstance(raw, dict):
        return None
    cost = raw.get("cost")
    if isinstance(cost, dict):
        cost = cost.get("total")
    usage = Usage(
        input=raw.get("input"),
        output=raw.get("output"),
        cache_read=raw.get("cache_read") or raw.get("cacheRead"),
        cache_write=raw.get("cache_write") or raw.get("cacheWrite"),
        total=raw.get("total"),
        cost=cost,
    )
    return None if usage.is_empty() else usage


def parse_session_file(path, *, tier, start_offset, prev_state, stat, lineage=None):
    data = scanutil.read_bytes(path)
    nl = data.find(b"\n")
    header_raw = data if nl == -1 else data[:nl]
    try:
        header = json.loads(header_raw.decode("utf-8", "replace"))
    except json.JSONDecodeError:
        raise scanutil.MalformedFileError(f"missing senpi session header in {path}")
    if not isinstance(header, dict) or header.get("type") != "session":
        raise scanutil.MalformedFileError(f"missing senpi session header in {path}")
    if header.get("version") not in SENPI_FORMAT_VERSIONS:
        raise scanutil.MalformedFileError(
            f"unsupported senpi session version {header.get('version')!r} in {path}"
        )
    native_id = header.get("id")
    if not native_id:
        raise scanutil.MalformedFileError(f"senpi session header lacks id in {path}")

    records, end_offset = scanutil.read_jsonl_records(
        data[start_offset:], start_offset=start_offset, path=path
    )

    source = _source_for(path)
    created_ms = iso_to_ms(header.get("timestamp")) or 0

    base_count = 0
    tail: list[SurfaceMessage] = []
    reused_full_windows = 0
    if start_offset and prev_state:
        base_count = int(prev_state.get("msg_count") or 0)
        reused_full_windows = int(prev_state.get("full_windows") or 0)
        tail = [SurfaceMessage.from_dict(d) for d in prev_state.get("tail") or []]

    provider = prev_state.get("provider") if prev_state else None
    model = prev_state.get("model") if prev_state else None
    usage = None
    if prev_state and isinstance(prev_state.get("usage"), dict):
        usage = Usage(**prev_state["usage"])
    first_prompt = prev_state.get("first_prompt") if prev_state else None
    updated_ms = int(prev_state.get("updated_at_ms") or 0) if prev_state else 0

    want_messages = tier == TIER_TRANSCRIPT
    new_messages: list[SurfaceMessage] = []
    new_count = 0
    next_seq = base_count

    for rec in records:
        if not isinstance(rec, dict):
            continue
        rtype = rec.get("type")
        if rtype == "session":
            continue
        if rtype == "model_change":
            provider = rec.get("provider") or provider
            model = rec.get("modelId") or model
            continue
        if rtype != "message":
            continue
        msg = rec.get("message") or {}
        role = ROLE_MAP.get(msg.get("role"))
        if role is None:
            continue
        text = scanutil.extract_text_blocks(msg.get("content"))
        if not text.strip():
            continue  # thinking-only / tool-call-only messages leave no surface
        new_count += 1
        ts = msg.get("timestamp")
        ts_ms = ts if isinstance(ts, int) else iso_to_ms(rec.get("timestamp"))
        if ts_ms:
            updated_ms = max(updated_ms, ts_ms)
        u = _normalize_usage(msg.get("usage"))
        if u is not None:
            usage = u
        if first_prompt is None and role == "user":
            first_prompt = redact.redact_first_prompt(text)
        if want_messages:
            new_messages.append(
                SurfaceMessage(
                    seq=next_seq,
                    role=role,
                    ts_ms=ts_ms,
                    text=redact.redact_message_text(text),
                )
            )
        next_seq += 1

    total = base_count + new_count
    full_windows = total // WINDOW
    combined = tail + new_messages
    new_tail = [m for m in combined if m.seq >= full_windows * WINDOW]

    if not updated_ms:
        updated_ms = created_ms

    session = NormalizedSession(
        session_uid=make_session_uid(source, native_id),
        source=source,
        native_id=native_id,
        root_session_id=native_id,
        parent_id=None,
        depth=0,
        cwd=header.get("cwd") or "",
        provider=provider,
        model=model,
        title=None,
        first_prompt=first_prompt,
        created_at_ms=created_ms,
        updated_at_ms=updated_ms,
        message_count=total,
        usage=usage,
        store_path=os.path.abspath(path),
        store_format="jsonl",
        messages=combined if want_messages else [],
    )
    if lineage:
        parent_native, root_native, depth = lineage
        session.parent_id = parent_native
        session.root_session_id = root_native or native_id
        session.depth = int(depth or 0)

    return scanutil.ParseOutcome(
        session=session,
        end_offset=end_offset,
        msg_count=total,
        full_windows=full_windows,
        tail=new_tail,
        reused_full_windows=reused_full_windows,
        extra_ckpt={
            "provider": provider,
            "model": model,
            "usage": usage.to_dict() if usage else None,
        },
    )


def load_tasks(tasks_roots) -> dict:
    """Map task_id -> (parent_session_id, root_session_id, depth)."""
    out = {}
    for root in tasks_roots or ():
        tasks_dir = os.path.join(root, "tasks")
        if not os.path.isdir(tasks_dir):
            continue
        for name in sorted(os.listdir(tasks_dir)):
            if not name.endswith(".json") or scanutil.is_denied(name):
                continue
            try:
                task = scanutil.read_json(os.path.join(tasks_dir, name))
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(task, dict):
                continue
            task_id = task.get("task_id") or name[:-5]
            out[task_id] = (
                task.get("parent_session_id"),
                task.get("root_session_id"),
                task.get("depth") or 0,
            )
    return out


def _lineage_for(path: str, task_map: dict):
    parts = path.split(os.sep)
    if "children" not in parts:
        return None
    idx = parts.index("children")
    if idx + 1 >= len(parts):
        return None
    return task_map.get(parts[idx + 1])


def scan(root, *, tier, checkpoint, prev_index, report, tasks_roots=()):
    task_map = load_tasks(tasks_roots)
    parsed = []
    for path in sorted(scanutil.walk_files(root)):
        if not path.endswith(".jsonl"):
            continue
        lineage = _lineage_for(path, task_map)
        pf = scanutil.drive_file(
            store=report.store,
            path=path,
            tier=tier,
            checkpoint=checkpoint,
            prev_index=prev_index,
            report=report,
            parse_fn=functools.partial(parse_session_file, lineage=lineage),
            append_only=True,
        )
        if pf is not None:
            parsed.append(pf)

    # subagent_count: tasks whose parent_session_id names a scanned session
    child_count: dict[str, int] = {}
    for parent_native, _root, _depth in task_map.values():
        if parent_native:
            child_count[parent_native] = child_count.get(parent_native, 0) + 1
    for pf in parsed:
        if pf.session is not None:
            pf.session.subagent_count = child_count.get(pf.session.native_id, 0)
    return parsed
