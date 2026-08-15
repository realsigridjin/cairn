"""Claude Code session store scanner.

Store layout (per st_01a005ba):
    ~/.claude/projects/<encoded-project>/<uuid>.jsonl          (sessions)
    ~/.claude/projects/<encoded-project>/sessions-index.json   (metadata index)
    ~/.claude/projects/<encoded-project>/**/subagents/*.jsonl  (subagents)

Metadata tier prefers ``sessions-index.json`` entries (summary, firstPrompt,
messageCount, gitBranch, projectPath, created/modified) and never opens the
transcript for indexed sessions. Transcript tier parses user/assistant lines;
sidechain lines inside a main transcript are skipped (they belong to
subagents). Subagent identity rule: the file's own ``agentId`` (fallback:
filename stem) is its native id; the embedded ``sessionId`` is the *parent*
and must never be used as the subagent's own identity.
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

SOURCE = "claude-code"


def _accumulate_usage(total: Usage, raw) -> None:
    if not isinstance(raw, dict):
        return
    inp = raw.get("input_tokens") or 0
    out = raw.get("output_tokens") or 0
    cr = raw.get("cache_read_input_tokens") or 0
    cw = raw.get("cache_creation_input_tokens") or 0
    total.input = (total.input or 0) + inp
    total.output = (total.output or 0) + out
    total.cache_read = (total.cache_read or 0) + cr
    total.cache_write = (total.cache_write or 0) + cw
    total.total = (total.total or 0) + inp + out + cr + cw


def _surface_from_message(role_hint, message) -> tuple[str, str] | None:
    """Map one user/assistant transcript message to (role, text) or None."""
    content = message.get("content")
    blocks = content if isinstance(content, list) else []
    block_types = {b.get("type") for b in blocks if isinstance(b, dict)}
    text = scanutil.extract_text_blocks(content)
    if not text.strip():
        return None
    if role_hint == "user" and "tool_result" in block_types and "text" not in block_types:
        return ("tool", text)
    if role_hint in ("user", "assistant"):
        return (role_hint, text)
    return None


def _session_from_index(path, native_id, entry, stat) -> NormalizedSession:
    created_ms = iso_to_ms(entry.get("created")) or entry.get("fileMtime") or 0
    updated_ms = iso_to_ms(entry.get("modified")) or created_ms
    project_path = entry.get("projectPath")
    session = NormalizedSession(
        session_uid=make_session_uid(SOURCE, native_id),
        source=SOURCE,
        native_id=native_id,
        root_session_id=native_id,
        cwd=project_path or "",
        repo_path=project_path,
        git_branch=entry.get("gitBranch"),
        title=redact.redact_title(entry.get("summary")),
        first_prompt=redact.redact_first_prompt(entry.get("firstPrompt")),
        created_at_ms=int(created_ms),
        updated_at_ms=int(updated_ms),
        message_count=int(entry.get("messageCount") or 0),
        store_path=os.path.abspath(path),
        store_format="jsonl",
    )
    return session


def _parse_transcript(path, *, native_id, tier, start_offset, prev_state, stat,
                      is_subagent):
    data = scanutil.read_bytes(path)
    records, end_offset = scanutil.read_jsonl_records(
        data[start_offset:], start_offset=start_offset, path=path
    )

    base_count = 0
    tail: list[SurfaceMessage] = []
    reused_full_windows = 0
    if start_offset and prev_state:
        base_count = int(prev_state.get("msg_count") or 0)
        reused_full_windows = int(prev_state.get("full_windows") or 0)
        tail = [SurfaceMessage.from_dict(d) for d in prev_state.get("tail") or []]

    cwd = prev_state.get("cwd") if prev_state else ""
    model = prev_state.get("model") if prev_state else None
    usage = Usage()
    have_usage = False
    if prev_state and isinstance(prev_state.get("usage"), dict):
        usage = Usage(**prev_state["usage"])
        have_usage = not usage.is_empty()
    first_prompt = prev_state.get("first_prompt") if prev_state else None
    created_ms = int(prev_state.get("created_at_ms") or 0) if prev_state else 0
    updated_ms = int(prev_state.get("updated_at_ms") or 0) if prev_state else 0
    parent_native = prev_state.get("parent_id") if prev_state else None

    want_messages = tier == TIER_TRANSCRIPT
    new_messages: list[SurfaceMessage] = []
    new_count = 0
    next_seq = base_count

    for rec in records:
        if not isinstance(rec, dict):
            continue
        rtype = rec.get("type")
        if rtype not in ("user", "assistant"):
            continue
        if not is_subagent and rec.get("isSidechain"):
            continue  # sidechain content belongs to the subagent projection
        message = rec.get("message") or {}
        if not isinstance(message, dict):
            continue
        mapped = _surface_from_message(rtype, message)
        if mapped is None:
            continue
        role, text = mapped
        new_count += 1
        ts_ms = iso_to_ms(rec.get("timestamp"))
        if ts_ms:
            if not created_ms:
                created_ms = ts_ms
            updated_ms = max(updated_ms, ts_ms)
        if rec.get("cwd"):
            cwd = rec["cwd"]
        if rtype == "assistant":
            if message.get("model"):
                model = message["model"]
            _accumulate_usage(usage, message.get("usage"))
            have_usage = have_usage or isinstance(message.get("usage"), dict)
        if parent_native is None and rec.get("sessionId") and is_subagent:
            parent_native = rec.get("sessionId")
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

    if not created_ms:
        created_ms = int(getattr(stat, "st_mtime", 0) * 1000)
    if not updated_ms:
        updated_ms = created_ms

    session = NormalizedSession(
        session_uid=make_session_uid(SOURCE, native_id),
        source=SOURCE,
        native_id=native_id,
        root_session_id=native_id,
        parent_id=parent_native if is_subagent else None,
        depth=0,
        cwd=cwd or "",
        model=model,
        first_prompt=first_prompt,
        created_at_ms=created_ms,
        updated_at_ms=updated_ms,
        message_count=total,
        usage=usage if have_usage and not usage.is_empty() else None,
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
            "cwd": session.cwd,
            "model": model,
            "created_at_ms": created_ms,
            "parent_id": session.parent_id,
            "usage": session.usage.to_dict() if session.usage else None,
        },
    )


def parse_main(path, *, tier, start_offset, prev_state, stat, index_entry=None):
    native_id = os.path.basename(path)[: -len(".jsonl")]
    if tier != TIER_TRANSCRIPT and index_entry is not None:
        # Metadata tier with index coverage: never open the transcript.
        session = _session_from_index(path, native_id, index_entry, stat)
        return scanutil.ParseOutcome(
            session=session,
            end_offset=stat.st_size,
            msg_count=session.message_count,
            full_windows=0,
            tail=[],
            reused_full_windows=0,
            extra_ckpt={"cwd": session.cwd, "created_at_ms": session.created_at_ms},
        )
    outcome = _parse_transcript(
        path,
        native_id=native_id,
        tier=tier,
        start_offset=start_offset,
        prev_state=prev_state,
        stat=stat,
        is_subagent=False,
    )
    if index_entry is not None:
        # Enrich transcript-derived metadata from the index.
        s = outcome.session
        s.title = redact.redact_title(index_entry.get("summary"))
        s.repo_path = index_entry.get("projectPath") or s.repo_path
        s.git_branch = index_entry.get("gitBranch") or s.git_branch
        if not s.cwd:
            s.cwd = index_entry.get("projectPath") or ""
        if s.first_prompt is None:
            s.first_prompt = redact.redact_first_prompt(index_entry.get("firstPrompt"))
    return outcome


def parse_subagent(path, *, tier, start_offset, prev_state, stat):
    agent_id = None
    parent_native = None
    # Identity comes from content: first agentId / sessionId pair wins.
    with scanutil.open_text(path) as fh:
        for line in fh:
            if not line.strip():
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                break
            if isinstance(rec, dict):
                agent_id = rec.get("agentId") or agent_id
                parent_native = rec.get("sessionId") or parent_native
            if agent_id and parent_native:
                break
    stem = os.path.basename(path)[: -len(".jsonl")]
    native_id = agent_id or stem
    if parent_native and parent_native == native_id:
        raise scanutil.MalformedFileError(
            f"subagent file {path} self-references sessionId; refusing identity"
        )
    outcome = _parse_transcript(
        path,
        native_id=native_id,
        tier=tier,
        start_offset=start_offset,
        prev_state=prev_state,
        stat=stat,
        is_subagent=True,
    )
    outcome.session.parent_id = outcome.session.parent_id or parent_native
    outcome.session.root_session_id = outcome.session.parent_id or native_id
    outcome.session.depth = 1  # refined in the scan post-pass
    return outcome


def _projects_root(root):
    if os.path.basename(root) != "projects" and os.path.isdir(
        os.path.join(root, "projects")
    ):
        return os.path.join(root, "projects")
    return root


def _project_dirs(root):
    proot = _projects_root(root)
    try:
        names = sorted(os.listdir(proot))
    except OSError:
        return []
    dirs = []
    if any(
        n.endswith(".jsonl") or n == "sessions-index.json"
        for n in names
        if not scanutil.is_denied(n)
    ):
        dirs.append(proot)
    for name in names:
        sub = os.path.join(proot, name)
        if os.path.isdir(sub) and not scanutil.is_denied(name):
            dirs.append(sub)
    return dirs


def _load_index(project_dir, report):
    index_path = os.path.join(project_dir, "sessions-index.json")
    if not os.path.exists(index_path):
        return {}
    try:
        data = scanutil.read_json(index_path)
    except (OSError, json.JSONDecodeError, scanutil.DeniedPathError) as exc:
        report.errors.append(
            scanutil.FileError(report.store, index_path, f"bad sessions-index.json: {exc}")
        )
        return {}
    entries = data.get("entries") if isinstance(data, dict) else None
    out = {}
    if isinstance(entries, list):
        for entry in entries:
            if isinstance(entry, dict) and entry.get("sessionId"):
                out[entry["sessionId"]] = entry
    return out


def scan(root, *, tier, checkpoint, prev_index, report):
    parsed = []
    for project_dir in _project_dirs(root):
        index_entries = _load_index(project_dir, report)
        main_files = []
        subagent_files = []
        try:
            names = sorted(os.listdir(project_dir))
        except OSError:
            continue
        for name in names:
            if scanutil.is_denied(name) or not name.endswith(".jsonl"):
                continue
            main_files.append(os.path.join(project_dir, name))
        for path in scanutil.walk_files(project_dir):
            parts = path.split(os.sep)
            if "subagents" in parts and path.endswith(".jsonl"):
                subagent_files.append(path)
        for path in main_files:
            stem = os.path.basename(path)[: -len(".jsonl")]
            pf = scanutil.drive_file(
                store=report.store,
                path=path,
                tier=tier,
                checkpoint=checkpoint,
                prev_index=prev_index,
                report=report,
                parse_fn=functools.partial(
                    parse_main, index_entry=index_entries.get(stem)
                ),
                append_only=True,
            )
            if pf is not None:
                parsed.append(pf)
        for path in sorted(subagent_files):
            pf = scanutil.drive_file(
                store=report.store,
                path=path,
                tier=tier,
                checkpoint=checkpoint,
                prev_index=prev_index,
                report=report,
                parse_fn=parse_subagent,
                append_only=True,
            )
            if pf is not None:
                parsed.append(pf)

    # Lineage post-pass: depth/root resolution and subagent counts.
    by_native = {pf.session.native_id: pf for pf in parsed if pf.session is not None}
    for pf in parsed:
        s = pf.session
        if s is None or s.parent_id is None:
            continue
        parent = by_native.get(s.parent_id)
        if parent is not None and parent.session is not None:
            s.depth = parent.session.depth + 1
            s.root_session_id = parent.session.root_session_id
            parent.session.subagent_count += 1
    return parsed
