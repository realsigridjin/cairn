"""Shared helpers for cairn_sessions tests.

Importing this module puts ``scripts/`` on sys.path so ``cairn_sessions``
and ``session_import`` are importable regardless of the unittest discovery
start directory.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

TESTS_DIR = pathlib.Path(__file__).resolve().parent
SCRIPTS_DIR = TESTS_DIR.parents[1]
REPO_ROOT = TESTS_DIR.parents[2]
FIXTURES = REPO_ROOT / "tests" / "fixtures" / "sessions"

if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

from cairn_sessions import checkpoint as checkpoint_mod  # noqa: E402
from cairn_sessions import emit, scanutil  # noqa: E402


def zstd_available() -> bool:
    try:
        from compression import zstd  # noqa: F401

        return True
    except ImportError:
        return shutil.which("zstd") is not None


def zstd_compress(data: bytes) -> bytes:
    try:
        from compression import zstd as pyzstd

        return pyzstd.compress(data)
    except ImportError:
        exe = shutil.which("zstd")
        proc = subprocess.run(
            [exe, "-q", "-c"], input=data, capture_output=True, check=True
        )
        return proc.stdout


def make_tmp() -> tempfile.TemporaryDirectory:
    return tempfile.TemporaryDirectory(prefix="cairn-sessions-test-")


def fresh_checkpoint(tmpdir: str) -> checkpoint_mod.Checkpoint:
    return checkpoint_mod.Checkpoint.load(os.path.join(tmpdir, "checkpoint.json"))


def empty_prev() -> emit.PrevCorpusIndex:
    return emit.PrevCorpusIndex.load(None)


def make_report(store: str, root: str) -> scanutil.StoreReport:
    return scanutil.StoreReport(store=store, root=root)


class OpenSpy:
    """Records every path opened through scanutil's safe IO wrappers."""

    def __init__(self):
        self.opened: list[str] = []
        self._orig_open_text = scanutil.open_text
        self._orig_read_bytes = scanutil.read_bytes

    def __enter__(self):
        spy = self

        def open_text(path, **kwargs):
            spy.opened.append(os.path.abspath(path))
            return spy._orig_open_text(path, **kwargs)

        def read_bytes(path):
            spy.opened.append(os.path.abspath(path))
            return spy._orig_read_bytes(path)

        scanutil.open_text = open_text
        scanutil.read_bytes = read_bytes
        return self

    def __exit__(self, *exc):
        scanutil.open_text = self._orig_open_text
        scanutil.read_bytes = self._orig_read_bytes
        return False

    def basenames(self) -> set[str]:
        return {os.path.basename(p) for p in self.opened}


def run_senpi_pipeline(root, *, tier, out, ckpt_path, tasks_roots=()):
    """Mini import pipeline over one senpi-format store."""
    prev = emit.PrevCorpusIndex.load(out)
    ckpt = checkpoint_mod.Checkpoint.load(ckpt_path)
    report = scanutil.StoreReport(store="senpi", root=str(root))
    from cairn_sessions import scan_senpi

    parsed = scan_senpi.scan(
        str(root),
        tier=tier,
        checkpoint=ckpt,
        prev_index=prev,
        report=report,
        tasks_roots=tasks_roots,
    )
    warnings: list[str] = []
    lines = emit.build_corpus(parsed, prev, warnings)
    emit.write_corpus(lines, str(out))
    ckpt.save()
    return report, lines, warnings


def senpi_session_lines(n_messages: int, session_id: str, cwd: str = "/tmp/incr") -> str:
    """Synthetic senpi session file content with ``n_messages`` messages."""
    rows = [
        '{"type":"session","version":3,"id":"%s","timestamp":"2026-08-01T00:00:00.000Z","cwd":"%s"}'
        % (session_id, cwd),
        '{"type":"model_change","id":"mc","parentId":null,"timestamp":"2026-08-01T00:00:01.000Z","provider":"p","modelId":"m"}',
    ]
    for i in range(n_messages):
        role = "user" if i % 2 == 0 else "assistant"
        rows.append(
            '{"type":"message","id":"m%d","parentId":"mc","timestamp":"2026-08-01T00:01:%02d.000Z",'
            '"message":{"role":"%s","content":[{"type":"text","text":"msg %d body"}],"timestamp":%d}}'
            % (i, i, role, i, 1786233660000 + i * 1000)
        )
    return "\n".join(rows) + "\n"


DSH_SESSION_ID = "session-99999999-9999-4999-8999-999999999999"


def build_dsh_store(tmp: str, *, session_file="session-good.jsonl",
                    with_projcache=True, with_workspace=True,
                    raw_bytes=None, session_id=DSH_SESSION_ID) -> str:
    """Assemble a synthetic DSH store; returns the store root path."""
    root = pathlib.Path(tmp) / "dsh-store"
    sdir = root / "--Users-test-dshproj--" / session_id
    sdir.mkdir(parents=True, exist_ok=True)
    if raw_bytes is not None:
        (sdir / "session.jsonl.zstd").write_bytes(raw_bytes)
    else:
        data = (FIXTURES / "dsh" / "raw" / session_file).read_bytes()
        (sdir / "session.jsonl.zstd").write_bytes(zstd_compress(data))
    raw_dir = FIXTURES / "dsh" / "raw"
    if with_projcache:
        shutil.copy(raw_dir / "session_projcache.json", sdir / "session_projcache.json")
    if with_workspace:
        shutil.copy(raw_dir / "workspace.json", sdir / "workspace.json")
    return str(root)
