"""BrowseComp-Plus retrieval-level evaluation.

BrowseComp-Plus exists to isolate the retriever from the LLM agent, and ships
human-verified gold documents per query. Retrieval recall is therefore
measurable here with no LLM and no API key; end-to-end answer accuracy is not,
and is not claimed.

Corpus construction: the union of every sampled query's gold + hard-negative
documents. Pooling across queries means each query is also scored against other
queries' documents as additional distractors, which is strictly harder than
scoring against its own pool alone.

Scoring is at DOCUMENT granularity, matching the benchmark's own metrics: a
chunk ranking is de-duplicated to first-occurrence document order before
recall@k is computed, so several chunks of one document cannot consume the
top-k slots.

Input must already be de-obfuscated (see decrypt_bcp.py); the published rows are
XOR-obfuscated and BM25 over ciphertext is meaningless.
"""

import concurrent.futures
import json
import pathlib
import subprocess
import sys

SUBSET = pathlib.Path(sys.argv[1])
BIN = sys.argv[2]
CONFIG = sys.argv[3]
MODE = sys.argv[4] if len(sys.argv) > 4 else "eval"
WINDOW = 1800
CHUNK_POOL = 100
DOC_K = 10
BUDGET = 300
WORKERS = 6
STUB = [0.25, 0.25, 0.25, 0.25]

rows = [json.loads(l) for l in SUBSET.read_text(encoding="utf-8").splitlines() if l.strip()]


def windows(text: str):
    for start in range(0, max(len(text), 1), WINDOW):
        piece = text[start : start + WINDOW].strip()
        if piece:
            yield start, piece


if MODE == "build":
    out_path = pathlib.Path(sys.argv[5])
    seen, chunks = set(), 0
    with out_path.open("w", encoding="utf-8") as out:
        for row in rows:
            for doc in row["gold_docs"] + row["negative_docs"]:
                docid = doc["docid"]
                if docid in seen:
                    continue
                seen.add(docid)
                for start, piece in windows(doc["text"]):
                    out.write(json.dumps({
                        "id": f"{docid}#c{start}",
                        "text": piece,
                        "vector": STUB,
                        "metadata": {"docid": docid},
                    }, ensure_ascii=False) + "\n")
                    chunks += 1
    print(f"queries={len(rows)} docs={len(seen)} chunks={chunks} out={out_path}")
    raise SystemExit(0)


# A ~100k-chunk web corpus with long natural-language queries reads far more
# than the CLI's 256 MiB default cold-path budget (measured: ~277 MiB/query),
# so the benchmark raises both budgets explicitly rather than silently
# truncating retrieval.
MAX_REMOTE_BYTES = 2 * 1024 * 1024 * 1024
MAX_RANGE_READS = 100_000


def query(text: str, *, limit: int, budget: int | None) -> dict:
    cmd = [BIN, "--config", CONFIG, "query", text, "--lexical-only",
           "--format", "json", "--limit", str(limit),
           "--max-remote-bytes", str(MAX_REMOTE_BYTES),
           "--max-range-reads", str(MAX_RANGE_READS)]
    if budget:
        cmd += ["--max-text-bytes", str(budget)]
    done = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if done.returncode != 0:
        return {"hits": [], "error": done.stderr.strip()[:200]}
    return json.loads(done.stdout)


def ranked_docs(resp: dict) -> list[str]:
    """First-occurrence document order from a chunk ranking."""
    order, seen = [], set()
    for hit in resp.get("hits", []):
        docid = hit["metadata"].get("docid")
        if docid is not None and docid not in seen:
            seen.add(docid)
            order.append(docid)
    return order


def doc_payload_bytes(resp: dict, keep: set[str]) -> int:
    """Bytes an agent would actually read for the top-k documents: the
    highest-ranked chunk of each kept document."""
    total, seen = 0, set()
    for hit in resp.get("hits", []):
        docid = hit["metadata"].get("docid")
        if docid in keep and docid not in seen:
            seen.add(docid)
            total += len(hit["text"].encode("utf-8"))
    return total


def evaluate(row: dict) -> dict:
    gold = {d["docid"] for d in row["gold_docs"]}
    base = query(row["query"], limit=CHUNK_POOL, budget=None)
    if "error" in base:
        return {"query_id": row["query_id"], "error": base["error"]}
    trimmed = query(row["query"], limit=CHUNK_POOL, budget=BUDGET)
    if "error" in trimmed:
        return {"query_id": row["query_id"], "error": trimmed["error"]}

    docs = ranked_docs(base)[:DOC_K]
    found = gold & set(docs)
    return {
        "query_id": row["query_id"],
        "recall_at_10_docs": len(found) / len(gold) if gold else 0.0,
        "gold": len(gold),
        "found": len(found),
        "base_bytes": doc_payload_bytes(base, set(docs)),
        "trim_bytes": doc_payload_bytes(trimmed, set(ranked_docs(trimmed)[:DOC_K])),
        "remote_bytes": base.get("remote_bytes", 0),
        "range_reads": base.get("range_reads", 0),
    }


# Each query reads hundreds of MiB of posting lists (CAIRN's cold lexical path
# has no WAND/BMW dynamic pruning), so the arms run concurrently.
results = []
with concurrent.futures.ThreadPoolExecutor(max_workers=WORKERS) as pool:
    futures = {pool.submit(evaluate, row): row["query_id"] for row in rows}
    for done_count, future in enumerate(concurrent.futures.as_completed(futures), 1):
        record = future.result()
        results.append(record)
        if "error" in record:
            print(f"[{done_count}/{len(rows)}] {record['query_id']} ERROR {record['error']}", flush=True)
        else:
            print(f"[{done_count}/{len(rows)}] {record['query_id']} "
                  f"recall@{DOC_K}docs={record['recall_at_10_docs']:.2f} "
                  f"({record['found']}/{record['gold']})", flush=True)

results.sort(key=lambda r: str(r["query_id"]))
pathlib.Path("/tmp/zgbench/bcp_results.json").write_text(
    json.dumps(results, indent=2), encoding="utf-8")

ok = [r for r in results if "error" not in r]
errors = len(results) - len(ok)
recalls = [r["recall_at_10_docs"] for r in ok]
base_total = sum(r["base_bytes"] for r in ok)
trim_total = sum(r["trim_bytes"] for r in ok)
n = max(len(recalls), 1)

print("\n================ BrowseComp-Plus retrieval evaluation ================")
print(f"queries evaluated             : {len(ok)} (errors: {errors})")
print(f"retrieval                     : BM25 lexical-only (NO embedding key)")
print(f"granularity                   : chunk pool {CHUNK_POOL} -> dedup -> top-{DOC_K} docs")
print(f"mean recall@{DOC_K} (gold docs)    : {sum(recalls) / n:.3f}")
print(f"queries with >=1 gold in top{DOC_K}: {sum(1 for r in recalls if r > 0)}/{len(recalls)}")
print(f"queries with all gold in top{DOC_K} : {sum(1 for r in recalls if r >= 1.0)}/{len(recalls)}")
if ok:
    print(f"mean cold read per query      : {sum(r['remote_bytes'] for r in ok) // len(ok)} bytes")
    print(f"mean range reads per query    : {sum(r['range_reads'] for r in ok) // len(ok)}")
print("--- max_text_bytes context budget (top-10 docs of one turn) ---")
print(f"payload bytes baseline        : {base_total}")
print(f"payload bytes @{BUDGET}B budget   : {trim_total}")
if base_total:
    print(f"reduction                     : {100 * (base_total - trim_total) / base_total:.2f}%")
print(f"approx input tokens baseline  : ~{base_total // 4}")
print(f"approx input tokens trimmed   : ~{trim_total // 4}")
print("=====================================================================")
