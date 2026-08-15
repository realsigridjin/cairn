"""Codex session store scanner.

Store layout (per st_01a005ba):
    ~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ISO-ts>-<uuid>.jsonl

First record is ``session_meta`` (id, cwd, model_provider, optional
thread_spawn / parent_thread_id lineage). Surface messages come from
``response_item`` message payloads (user/assistant) and
``function_call_output`` payloads (tool); ``function_call`` arguments,
reasoning items and developer instructions are structural drops.
Token usage comes from ``event_msg``/``token_count`` payloads.
"""

from __future__ import annotations

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

SOURCE = "codex"

_SURFACE_ROLES = {"user": "user", "assistant": "assistant"}


def _usage_from_token_count(payload: dict) -> Usage | None:
    info = payload.get("info") if isinstance(payload.get("info"), dict) else payload
    totals = info.get("total_token_usage") if isinstance(info, dict) else None
    if not isinstance(totals, dict):
        totals = info if isinstance(info, dict) else {}
    usage = Usage(
        input=totals.get("input_tokens") or totals.get("input"),
        output=totals.get("output_tokens") or totals.get("output"),
        cache_read=totals.get("cached_input_tokens") or totals.get("cache_read"),
        cache_write=totals.get("cache_write"),
        total=totals.get("total_tokens") or totals.get("total"),
        cost=totals.get("cost"),
    )
    return None if usage.is_empty() else usage


def parse_rollout(path, *, tier, start_offset, prev_state, stat):
    data = scanutil.read_bytes(path)
    nl = data.find(b"\n")
    header_raw = data if nl == -1 else data[:nl]
    try:
        header = json.loads(header_raw.decode("utf-8", "replace"))
    except json.JSONDecodeError:
        raise scanutil.MalformedFileError(f"missing codex session_meta header in {path}")
    if not isinstance(header, dict) or header.get("type") != "session_meta":
        raise scanutil.MalformedFileError(f"missing codex session_meta header in {path}")
    payload = header.get("payload")
    if not isinstance(payload, dict):
        raise scanutil.MalformedFileError(f"codex session_meta lacks payload in {path}")
    native_id = payload.get("id") or payload.get("session_id")
    if not native_id:
        raise scanutil.MalformedFileError(f"codex session_meta lacks id in {path}")

    records, end_offset = scanutil.read_jsonl_records(
        data[start_offset:], start_offset=start_offset, path=path
    )

    created_ms = iso_to_ms(payload.get("timestamp")) or iso_to_ms(header.get("timestamp")) or 0

    base_count = 0
    tail: list[SurfaceMessage] = []
    reused_full_windows = 0
    if start_offset and prev_state:
        base_count = int(prev_state.get("msg_count") or 0)
        reused_full_windows = int(prev_state.get("full_windows") or 0)
        tail = [SurfaceMessage.from_dict(d) for d in prev_state.get("tail") or []]

    provider = payload.get("model_provider")
    model = prev_state.get("model") if prev_state else None
    cwd = payload.get("cwd") or (prev_state.get("cwd") if prev_state else "") or ""
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
        ts_ms = iso_to_ms(rec.get("timestamp"))
        if rtype == "turn_context":
            tpayload = rec.get("payload") or {}
            if isinstance(tpayload, dict):
                model = tpayload.get("model") or model
                cwd = tpayload.get("cwd") or cwd
            continue
        if rtype == "event_msg":
            epayload = rec.get("payload") or {}
            if isinstance(epayload, dict) and epayload.get("type") == "token_count":
                u = _usage_from_token_count(epayload)
                if u is not None:
                    usage = u
            continue
        if rtype != "response_item":
            continue
        item = rec.get("payload") or {}
        if not isinstance(item, dict):
            continue
        itype = item.get("type")
        if itype == "message":
            role = _SURFACE_ROLES.get(item.get("role"))
            if role is None:
                continue  # developer/system instructions are structural drops
            text = scanutil.extract_text_blocks(item.get("content"))
        elif itype == "function_call_output":
            role = "tool"
            output = item.get("output")
            text = output if isinstance(output, str) else json.dumps(output or "")
        else:
            continue  # function_call arguments, reasoning, etc. are dropped
        if not text.strip():
            continue
        new_count += 1
        if ts_ms:
            updated_ms = max(updated_ms, ts_ms)
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

    thread_spawn = payload.get("thread_spawn") if isinstance(payload.get("thread_spawn"), dict) else {}
    parent_native = payload.get("parent_thread_id")
    depth = int(thread_spawn.get("depth") or 0) if thread_spawn else 0
    root_native = payload.get("root_thread_id") or parent_native or native_id

    session = NormalizedSession(
        session_uid=make_session_uid(SOURCE, native_id),
        source=SOURCE,
        native_id=native_id,
        root_session_id=root_native,
        parent_id=parent_native,
        depth=depth,
        agent_role=payload.get("agent_role") or payload.get("originator"),
        cwd=cwd,
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

    return scanutil.ParseOutcome(
        session=session,
        end_offset=end_offset,
        msg_count=total,
        full_windows=full_windows,
        tail=new_tail,
        reused_full_windows=reused_full_windows,
        extra_ckpt={
            "model": model,
            "cwd": cwd,
            "usage": usage.to_dict() if usage else None,
        },
    )


def scan(root, *, tier, checkpoint, prev_index, report):
    parsed = []
    for path in sorted(scanutil.walk_files(root)):
        name = os.path.basename(path)
        if not (name.startswith("rollout-") and name.endswith(".jsonl")):
            continue
        pf = scanutil.drive_file(
            store=report.store,
            path=path,
            tier=tier,
            checkpoint=checkpoint,
            prev_index=prev_index,
            report=report,
            parse_fn=parse_rollout,
            append_only=True,
        )
        if pf is not None:
            parsed.append(pf)
    return parsed
