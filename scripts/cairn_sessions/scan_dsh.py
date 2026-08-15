"""DeepSeek Harness session store scanner.

Store layout (per st_01a005b9):
    ~/.dsh/sessions/--<encoded-cwd>--/session-<uuid>/session.jsonl.zstd
    ~/.dsh/sessions/--<encoded-cwd>--/session-<uuid>/session_projcache.json
    ~/.dsh/sessions/--<encoded-cwd>--/session-<uuid>/workspace.json

The transcript is zstd-framed JSONL, format version 0 ("no compatibility
implied"): an unknown header version is a store-level hard stop, mirroring
DSH's own refusal policy. Decompression uses the Python 3.14 stdlib
``compression.zstd`` module with a ``zstd -dc`` CLI fallback.

Metadata tier is served from ``session_projcache.json`` + ``workspace.json``
without decompressing the transcript when the projcache is complete.
Transcript tier derives the surface projection (``user/message``,
``assistant/message``, ``tool/result``) only; thinking, tool arguments and
other event payloads are structural drops.
"""

from __future__ import annotations

import functools
import json
import os
import shutil
import subprocess

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

SOURCE = "dsh"
DSH_FORMAT_VERSIONS = frozenset({0})

SURFACE_EVENT_ROLES = {
    "user/message": "user",
    "assistant/message": "assistant",
    "tool/result": "tool",
}

_PROJCACHE_REQUIRED = ("title", "createdAt", "updatedAt", "messageCount", "tokenUsage", "identity")


def decompress_zstd(data: bytes, *, backend: str = "auto", zstd_bin: str | None = None) -> bytes:
    """Decompress one zstd frame via stdlib (3.14+) or the zstd CLI."""
    if backend in ("auto", "stdlib"):
        try:
            from compression import zstd as pyzstd
        except ImportError:
            if backend == "stdlib":
                raise scanutil.MalformedFileError(
                    "python stdlib compression.zstd unavailable"
                )
        else:
            try:
                return pyzstd.decompress(data)
            except pyzstd.ZstdError as exc:
                raise scanutil.MalformedFileError(f"bad zstd frame: {exc}")
    exe = zstd_bin or shutil.which("zstd")
    if not exe:
        raise scanutil.MalformedFileError("no zstd decoder available (stdlib or CLI)")
    proc = subprocess.run(
        [exe, "-dc"], input=data, capture_output=True, timeout=120, check=False
    )
    if proc.returncode != 0:
        tail = proc.stderr.decode("utf-8", "replace").strip().splitlines()
        raise scanutil.MalformedFileError(
            "bad zstd frame: zstd CLI: " + (tail[-1] if tail else f"exit {proc.returncode}")
        )
    return proc.stdout


def _usage_from_projcache(raw) -> Usage | None:
    if not isinstance(raw, dict):
        return None
    usage = Usage(
        input=raw.get("input") or raw.get("input_tokens"),
        output=raw.get("output") or raw.get("output_tokens"),
        cache_read=raw.get("cache_read") or raw.get("cache_read_input_tokens"),
        cache_write=raw.get("cache_write") or raw.get("cache_creation_input_tokens"),
        total=raw.get("total") or raw.get("total_tokens"),
        cost=raw.get("cost"),
    )
    return None if usage.is_empty() else usage


def _load_json_quiet(path):
    try:
        return scanutil.read_json(path)
    except (OSError, json.JSONDecodeError, scanutil.DeniedPathError):
        return None


def _projcache_complete(cache: dict) -> bool:
    return all(key in cache for key in _PROJCACHE_REQUIRED) and isinstance(
        cache.get("identity"), dict
    )


def _session_from_projcache(native_id, zstd_path, cache, workspace, stat):
    identity = cache.get("identity") or {}
    created_ms = cache.get("createdAt") or 0
    updated_ms = cache.get("updatedAt") or created_ms
    session = NormalizedSession(
        session_uid=make_session_uid(SOURCE, native_id),
        source=SOURCE,
        native_id=native_id,
        root_session_id=native_id,
        parent_id=cache.get("parentSession"),
        depth=int(cache.get("delegationDepth") or 0),
        agent_role=cache.get("agentPreset") or identity.get("agentPreset"),
        cwd=identity.get("cwd") or "",
        repo_path=(workspace or {}).get("path"),
        provider=identity.get("provider"),
        model=identity.get("model"),
        title=redact.redact_title(cache.get("title")),
        first_prompt=redact.redact_first_prompt(cache.get("firstPrompt")),
        created_at_ms=int(created_ms),
        updated_at_ms=int(updated_ms),
        message_count=int(cache.get("messageCount") or 0),
        usage=_usage_from_projcache(cache.get("tokenUsage")),
        store_path=os.path.abspath(zstd_path),
        store_format="jsonl.zstd",
    )
    return scanutil.ParseOutcome(
        session=session,
        end_offset=stat.st_size,
        msg_count=session.message_count,
        full_windows=0,
        tail=[],
        reused_full_windows=0,
        extra_ckpt={"cwd": session.cwd, "created_at_ms": session.created_at_ms},
    )


def _event_text(role, data) -> str:
    if not isinstance(data, dict):
        return ""
    if role == "tool":
        output = data.get("output")
        if isinstance(output, str) and output.strip():
            return output
    return scanutil.extract_text_blocks(data.get("content"))


def parse_session(path, *, tier, start_offset, prev_state, stat,
                  zstd_backend="auto", zstd_bin=None):
    session_dir = os.path.dirname(path)
    native_id = os.path.basename(session_dir)

    cache = None
    cache_path = os.path.join(session_dir, "session_projcache.json")
    if os.path.exists(cache_path):
        loaded = _load_json_quiet(cache_path)
        if isinstance(loaded, dict):
            cache = loaded
    workspace = None
    workspace_path = os.path.join(session_dir, "workspace.json")
    if os.path.exists(workspace_path):
        loaded = _load_json_quiet(workspace_path)
        if isinstance(loaded, dict):
            workspace = loaded

    if tier != TIER_TRANSCRIPT and cache is not None and _projcache_complete(cache):
        return _session_from_projcache(native_id, path, cache, workspace, stat)

    raw = scanutil.read_bytes(path)
    data = decompress_zstd(raw, backend=zstd_backend, zstd_bin=zstd_bin)
    records, _end = scanutil.read_jsonl_records(data, start_offset=0, path=path)
    if not records:
        raise scanutil.MalformedFileError(f"empty dsh session stream in {path}")
    header = records[0]
    if not isinstance(header, dict) or header.get("type") != "session":
        raise scanutil.MalformedFileError(f"missing dsh session header in {path}")
    if header.get("version") not in DSH_FORMAT_VERSIONS:
        # Store-level hard stop, mirroring DSH's own refusal policy.
        raise scanutil.StoreHardError(
            f"unsupported dsh session format version {header.get('version')!r} in {path}"
        )

    native_id = header.get("id") or native_id
    created_ms = header.get("createdAt") or iso_to_ms(header.get("createdAt")) or 0
    updated_ms = int(created_ms)
    first_prompt = None
    usage = _usage_from_projcache((cache or {}).get("tokenUsage"))

    want_messages = tier == TIER_TRANSCRIPT
    messages: list[SurfaceMessage] = []
    total = 0
    for rec in records[1:]:
        if not isinstance(rec, dict):
            continue
        role = SURFACE_EVENT_ROLES.get(rec.get("type"))
        if role is None:
            continue
        text = _event_text(role, rec.get("data"))
        if not text.strip():
            continue
        ts_ms = rec.get("time")
        ts_ms = int(ts_ms) if isinstance(ts_ms, (int, float)) else None
        if ts_ms:
            updated_ms = max(updated_ms, ts_ms)
        if first_prompt is None and role == "user":
            first_prompt = redact.redact_first_prompt(text)
        if want_messages:
            messages.append(
                SurfaceMessage(
                    seq=total,
                    role=role,
                    ts_ms=ts_ms,
                    text=redact.redact_message_text(text),
                )
            )
        total += 1

    full_windows = total // WINDOW
    tail = [m for m in messages if m.seq >= full_windows * WINDOW]

    identity = (cache or {}).get("identity") or {}
    session = NormalizedSession(
        session_uid=make_session_uid(SOURCE, native_id),
        source=SOURCE,
        native_id=native_id,
        root_session_id=native_id,
        parent_id=header.get("parentSession") or (cache or {}).get("parentSession"),
        depth=int(header.get("delegationDepth") or 0),
        agent_role=header.get("agentPreset") or identity.get("agentPreset"),
        cwd=header.get("cwd") or identity.get("cwd") or "",
        repo_path=(workspace or {}).get("path"),
        provider=identity.get("provider"),
        model=identity.get("model"),
        title=redact.redact_title((cache or {}).get("title")),
        first_prompt=first_prompt,
        created_at_ms=int(created_ms),
        updated_at_ms=updated_ms,
        message_count=total,
        usage=usage,
        store_path=os.path.abspath(path),
        store_format="jsonl.zstd",
        messages=messages if want_messages else [],
    )
    return scanutil.ParseOutcome(
        session=session,
        end_offset=stat.st_size,  # zstd is not seekable: consumed wholesale
        msg_count=total,
        full_windows=full_windows,
        tail=tail,
        reused_full_windows=0,
        extra_ckpt={"cwd": session.cwd, "created_at_ms": session.created_at_ms},
    )


def scan(root, *, tier, checkpoint, prev_index, report,
         zstd_backend="auto", zstd_bin=None):
    parsed = []
    for path in sorted(scanutil.walk_files(root)):
        if os.path.basename(path) != "session.jsonl.zstd":
            continue
        pf = scanutil.drive_file(
            store=report.store,
            path=path,
            tier=tier,
            checkpoint=checkpoint,
            prev_index=prev_index,
            report=report,
            parse_fn=functools.partial(
                parse_session, zstd_backend=zstd_backend, zstd_bin=zstd_bin
            ),
            append_only=False,  # zstd frames are re-decoded wholesale on change
        )
        if pf is not None:
            parsed.append(pf)

    by_native = {pf.session.native_id: pf for pf in parsed if pf.session is not None}
    for pf in parsed:
        s = pf.session
        if s is None or s.parent_id is None:
            continue
        parent = by_native.get(s.parent_id)
        if parent is not None and parent.session is not None:
            parent.session.subagent_count += 1
            s.root_session_id = parent.session.root_session_id
    return parsed
