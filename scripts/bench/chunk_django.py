"""Chunk the Django checkout into a CAIRN ingest JSONL.

Offline mode: this environment has no embedding API key, so every chunk gets an
identical stub vector and all queries run --lexical-only. The stub vector is
inert for ranking (identical for all docs), so BM25 alone decides the order.
"""

import json
import pathlib
import sys

ROOT = pathlib.Path(sys.argv[1])
OUT = pathlib.Path(sys.argv[2])
WINDOW = 60
STUB = [0.25, 0.25, 0.25, 0.25]
MAX_TEXT_BYTES = 12000

files = sorted(p for p in ROOT.rglob("*.py") if ".git" not in p.parts)
chunks = 0
with OUT.open("w", encoding="utf-8") as out:
    for path in files:
        try:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        except OSError:
            continue
        rel = path.relative_to(ROOT).as_posix()
        for start in range(0, max(len(lines), 1), WINDOW):
            body = "\n".join(lines[start : start + WINDOW]).strip()
            if not body:
                continue
            body = body.encode("utf-8")[:MAX_TEXT_BYTES].decode("utf-8", "ignore")
            record = {
                "id": f"{rel}#L{start + 1}",
                "text": f"{rel}\n{body}",
                "vector": STUB,
                "metadata": {"path": rel, "start_line": start + 1},
            }
            out.write(json.dumps(record, ensure_ascii=False) + "\n")
            chunks += 1

print(f"files={len(files)} chunks={chunks} out={OUT}")
