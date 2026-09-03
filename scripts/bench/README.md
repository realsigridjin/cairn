# Retrieval benchmarks for the zg-derived capabilities

These harnesses measure the two capabilities ported from Qwen's `zg`
(zvec-grep) into CAIRN: the exact-string verification leg (`require_text`)
and the response-side context budget (`max_text_bytes`).

## Scope, stated up front

These are **retrieval-level** benchmarks, not agent-level ones.

zg's published headline numbers (Judge score, answer accuracy, tool-call
count, agent wall-clock) require an LLM agent plus an LLM judge. Reproducing
those requires model API credentials and is out of scope for these scripts;
they are neither estimated nor simulated here. What these scripts do measure,
on the real public datasets, is the retrieval layer those agent-level numbers
are downstream of:

- **precision of the returned pool** - how many returned hits actually contain
  the literal the question names. Every returned hit that lacks it is a read an
  agent must spend and discard.
- **gold-file / gold-doc hit rate** - whether the correct source is surfaced.
- **context cost of one retrieval turn**, in UTF-8 bytes.

Both harnesses run `--lexical-only` (BM25). Without an embedding key the
vectors are inert stubs, so BM25 alone decides the ranking. Report the
retrieval mode alongside any number produced here.

## SWE-QA-Bench (django split)

Dataset: `swe-qa/SWE-QA-Benchmark`, split `django` (48 questions).
Corpus: a real `django/django` checkout.

```sh
git clone --depth 1 https://github.com/django/django.git /tmp/zgbench/django
python3 scripts/bench/chunk_django.py /tmp/zgbench/django /tmp/zgbench/django_chunks.jsonl
cairn init --tenant bench --kb django --path /tmp/zgbench/kb/config.toml --local-store /tmp/zgbench/kb/store
cairn --config /tmp/zgbench/kb/config.toml ingest /tmp/zgbench/django_chunks.jsonl \
  --no-embed --embedding-provider external --embedding-model offline-stub --dev-calibration
python3 scripts/bench/eval_swe_qa.py "$(command -v cairn)" /tmp/zgbench/kb/config.toml \
  /tmp/zgbench/swe_qa_django.jsonl /tmp/zgbench/swe_qa_results.json
```

Fairness rule enforced by the script: the `require_text` literal is extracted
from the **question only** (the CamelCase symbol the asker already named), never
from the reference answer. Gold file paths parsed out of the reference answer
are used solely as ground truth for whether the right file was surfaced.

## BrowseComp-Plus

Dataset: `Tevatron/browsecomp-plus` (830 queries). Each row ships
human-verified `gold_docs` plus mined hard `negative_docs`, which is exactly
what makes retrieval recall measurable without an LLM.

Every field except `query_id` ships XOR-obfuscated against a SHA-256 keystream
derived from the published canary, so the benchmark stays out of training
corpora. `decrypt_bcp.py` mirrors the de-obfuscation snippet from the dataset
card; running BM25 against the raw rows would score ciphertext and is
meaningless.

```sh
python3 scripts/bench/fetch_bcp.py 80 /tmp/zgbench/bcp_subset.jsonl
python3 scripts/bench/decrypt_bcp.py /tmp/zgbench/bcp_subset.jsonl /tmp/zgbench/bcp_plain.jsonl
python3 scripts/bench/eval_bcp.py /tmp/zgbench/bcp_plain.jsonl x x build /tmp/zgbench/bcp_chunks.jsonl
# ingest bcp_chunks.jsonl the same way as the django corpus, then:
python3 scripts/bench/eval_bcp.py /tmp/zgbench/bcp_plain.jsonl "$(command -v cairn)" \
  /tmp/zgbench/bcp_kb/config.toml eval
```

The corpus is the pooled union of every sampled query's gold and negative
documents, so each query is scored against other queries' documents as
additional distractors - strictly harder than scoring against its own pool.

### Shard the web corpus

A single shard holds one directory entry per lexical term. A ~6k-document web
corpus has a far larger vocabulary than a code corpus, and ingesting all 120k
chunks into `--shards 1` fails with `directory exceeds format limit`
(`MAX_DIRECTORY_BYTES` = 64 MiB, src/binary.rs). Ingest this corpus with
`--shards 16`. The django corpus fits in one shard.

`require_text` is deliberately **not** exercised as a win on BrowseComp-Plus:
those queries are open-ended entity discovery with no identifier to verify,
which is precisely the case where the shipped tool description tells an agent
to leave it unset.

## CAIRN limits these benchmarks surfaced

Running real public corpora through the CLI exposed three operational limits
worth knowing before pointing CAIRN at a large web corpus. None of them are
caused by the `require_text` / `max_text_bytes` work; they are pre-existing
properties of the cold path.

1. **Single-shard directory ceiling.** A shard's directory carries one entry per
   lexical term. Ingesting 120,622 web-document chunks with `--shards 1` fails
   with `directory exceeds format limit` (`MAX_DIRECTORY_BYTES` = 64 MiB,
   src/binary.rs). `--shards 16` succeeds. The error names the format limit but
   not the remedy, so shard count is worth documenting for large corpora.

2. **Cold read budget is tuned for short queries.** BrowseComp-Plus queries are
   long multi-constraint natural-language questions. Against a 120k-chunk,
   16-shard corpus a single query reads ~600 MiB of posting lists and exceeds
   the 256 MiB CLI default, failing with `remote byte budget exceeded`. These
   scripts raise `--max-remote-bytes` and `--max-range-reads` explicitly.

3. **No dynamic pruning on the lexical path.** BM25 candidate generation reads
   every matching posting list in full; there is no WAND/BMW skipping (the
   accumulator guard in src/search/cold.rs says as much). Cost therefore scales
   with query length times corpus vocabulary, which is what makes (2) happen.

## Interpreting the BrowseComp-Plus numbers

BrowseComp queries are deliberately obfuscated: the question describes an
entity by indirect constraints and shares almost no vocabulary with its
evidence document. Lexical BM25 is close to the floor on this benchmark by
construction - it is a dense-retrieval benchmark. Without an embedding key the
retrieval half of these numbers measures the floor, not CAIRN's hybrid
retrieval. The context-budget half is unaffected, because it measures the bytes
a retrieval turn returns regardless of how the ranking was produced.
