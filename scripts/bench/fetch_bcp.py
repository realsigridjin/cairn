"""Fetch a BrowseComp-Plus subset (query + gold/negative doc pool) via the HF
datasets-server. Each row carries its own labelled pool, so a per-query
retrieval evaluation needs no bulk corpus download."""

import json
import sys
import time
import urllib.request

DATASET = "Tevatron/browsecomp-plus"
URL = (
    "https://datasets-server.huggingface.co/rows"
    f"?dataset={DATASET}&config=default&split=test&offset={{off}}&length=1"
)
WANT = int(sys.argv[1]) if len(sys.argv) > 1 else 80
OUT = sys.argv[2] if len(sys.argv) > 2 else "/tmp/zgbench/bcp_subset.jsonl"


def fetch(offset: int) -> dict:
    req = urllib.request.Request(
        URL.format(off=offset), headers={"user-agent": "cairn-bench/1.0"}
    )
    with urllib.request.urlopen(req, timeout=180) as resp:
        return json.loads(resp.read())


written = 0
with open(OUT, "w", encoding="utf-8") as out:
    for offset in range(WANT):
        for attempt in range(4):
            try:
                payload = fetch(offset)
                rows = payload.get("rows", [])
                if not rows:
                    raise RuntimeError(f"no rows at {offset}")
                row = rows[0]["row"]
                record = {
                    "query_id": row["query_id"],
                    "query": row["query"],
                    "answer": row["answer"],
                    "gold_docs": [
                        {"docid": d["docid"], "text": d["text"]}
                        for d in row.get("gold_docs") or []
                    ],
                    "negative_docs": [
                        {"docid": d["docid"], "text": d["text"]}
                        for d in row.get("negative_docs") or []
                    ],
                }
                out.write(json.dumps(record, ensure_ascii=False) + "\n")
                out.flush()
                written += 1
                print(
                    f"[{written}/{WANT}] {record['query_id']} "
                    f"gold={len(record['gold_docs'])} neg={len(record['negative_docs'])}",
                    flush=True,
                )
                break
            except Exception as error:  # noqa: BLE001 - report and retry
                print(f"  offset={offset} attempt={attempt} error={error}", flush=True)
                time.sleep(3 * (attempt + 1))
        else:
            print(f"GIVING UP on offset {offset}", flush=True)

print(f"DONE wrote={written} -> {OUT}", flush=True)
