"""SWE-QA-Bench (django) retrieval-level A/B for the ported zg capabilities.

Scope, stated honestly: this measures RETRIEVAL, not an agent. This environment
has no LLM API key, so Judge score / tool calls / agent runtime cannot be
reproduced. What is measured here are the two mechanisms the port adds, on the
real Django tree and the real SWE-QA django questions:

  A. require_text  -> precision of the returned pool (how many returned hits
     actually contain the identifier the question names). Every returned hit
     that lacks it is a read an agent must spend and discard.
  B. max_text_bytes -> context cost of one retrieval turn, in bytes.

Fairness: the require_text literal is extracted from the QUESTION only, never
from the reference answer, so the treatment arm gets no oracle knowledge. Gold
file paths from the reference answer are used solely as ground truth for
whether the correct file was surfaced.
"""

import json
import pathlib
import re
import statistics
import subprocess
import sys

BIN = sys.argv[1]
CONFIG = sys.argv[2]
QUESTIONS = pathlib.Path(sys.argv[3])
OUT = pathlib.Path(sys.argv[4])
LIMIT = 10
BUDGET = 300

STOP = {
    "Django", "What", "How", "Why", "When", "Where", "Which", "The", "This",
    "Python", "True", "False", "None", "API", "SQL", "DDL", "URL", "HTTP",
}
CAMEL = re.compile(r"\b[A-Z][a-z]+(?:[A-Z][a-zA-Z0-9]*)+\b")
GOLD_PATH = re.compile(r"\b[\w./-]+\.py\b")


def identifier(question: str) -> str | None:
    """Longest CamelCase symbol named by the question itself."""
    hits = [c for c in CAMEL.findall(question) if c not in STOP]
    return max(hits, key=len) if hits else None


def query(text: str, *, require: str | None, budget: int | None) -> dict:
    cmd = [
        BIN, "--config", CONFIG, "query", text,
        "--lexical-only", "--format", "json", "--limit", str(LIMIT),
    ]
    if require:
        cmd += ["--require-text", require]
    if budget:
        cmd += ["--max-text-bytes", str(budget)]
    done = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
    if done.returncode != 0:
        return {"hits": [], "error": done.stderr.strip()[:200]}
    return json.loads(done.stdout)


def payload_bytes(response: dict) -> int:
    return sum(len(h["text"].encode("utf-8")) for h in response.get("hits", []))


rows = [json.loads(line) for line in QUESTIONS.read_text(encoding="utf-8").splitlines() if line.strip()]
records = []

for index, row in enumerate(rows, 1):
    question, answer = row["question"], row["answer"]
    symbol = identifier(question)
    gold_paths = {p for p in GOLD_PATH.findall(answer) if "/" in p}

    base = query(question, require=None, budget=None)
    base_hits = base.get("hits", [])
    trimmed = query(question, require=None, budget=BUDGET)

    record = {
        "n": index,
        "symbol": symbol,
        "gold_paths": sorted(gold_paths),
        "base_hits": len(base_hits),
        "base_bytes": payload_bytes(base),
        "trimmed_bytes": payload_bytes(trimmed),
        "base_precision": None,
        "verified_hits": None,
        "verified_precision": None,
        "base_gold_hit": any(
            any(h["metadata"].get("path") == g for g in gold_paths) for h in base_hits
        ),
    }

    if symbol and base_hits:
        needle = symbol.lower()
        record["base_precision"] = sum(
            needle in h["text"].lower() for h in base_hits
        ) / len(base_hits)
        ver = query(question, require=symbol, budget=None)
        ver_hits = ver.get("hits", [])
        record["verified_hits"] = len(ver_hits)
        if ver_hits:
            record["verified_precision"] = sum(
                needle in h["text"].lower() for h in ver_hits
            ) / len(ver_hits)
            record["verified_gold_hit"] = any(
                any(h["metadata"].get("path") == g for g in gold_paths) for h in ver_hits
            )
    records.append(record)
    print(
        f"[{index}/{len(rows)}] sym={symbol} base={record['base_hits']} "
        f"prec={record['base_precision']} ver={record['verified_hits']} "
        f"vprec={record['verified_precision']}",
        flush=True,
    )

OUT.write_text(json.dumps(records, indent=2), encoding="utf-8")


def mean(values):
    values = [v for v in values if v is not None]
    return statistics.mean(values) if values else float("nan")


scored = [r for r in records if r["base_precision"] is not None]
base_bytes = sum(r["base_bytes"] for r in records)
trim_bytes = sum(r["trimmed_bytes"] for r in records)

print("\n================ SWE-QA-Bench (django) retrieval A/B ================")
print(f"questions                     : {len(records)}")
print(f"questions naming a symbol     : {len(scored)}")
print(f"corpus                        : 2930 django .py files -> 10048 chunks")
print(f"retrieval                     : BM25 lexical-only (no embedding key)")
print("--- A. require_text verification (top-10 pool) ---")
print(f"precision@10 baseline         : {mean(r['base_precision'] for r in scored):.3f}")
print(f"precision@10 with require_text: {mean(r['verified_precision'] for r in scored):.3f}")
wasted_base = mean((1 - r["base_precision"]) * r["base_hits"] for r in scored)
wasted_ver = mean(
    (1 - r["verified_precision"]) * r["verified_hits"]
    for r in scored if r["verified_precision"] is not None
)
print(f"irrelevant hits/query baseline: {wasted_base:.2f}")
print(f"irrelevant hits/query verified: {wasted_ver:.2f}")
print(f"pool size baseline            : {mean(r['base_hits'] for r in scored):.2f}")
print(f"pool size verified            : {mean(r['verified_hits'] for r in scored):.2f}")
print("--- B. max_text_bytes context budget ---")
print(f"payload bytes baseline        : {base_bytes}")
print(f"payload bytes @{BUDGET}B budget   : {trim_bytes}")
print(f"reduction                     : {100 * (base_bytes - trim_bytes) / base_bytes:.2f}%")
print(f"approx input tokens baseline  : ~{base_bytes // 4}")
print(f"approx input tokens trimmed   : ~{trim_bytes // 4}")
print("=====================================================================")
