# CAIRN 1.0

CAIRN 1.0 is the **revisioned retrieval and cross-agent memory layer for DeepSeek Harness**. It is an object-store-native retrieval runtime that keeps durable search artifacts in an object store, serves cold knowledge bases with ranged reads, and can optionally promote hot revisions into a local UQA-RS database. For DeepSeek Harness it ships as a native Cordis tool bundle: the agent gets one bounded, read-only retrieval tool, and every answer is pinned to an immutable revision with `cairn://` citations.

That revision pinning is what makes CAIRN a memory layer, not just a search box. A Harness session can record exactly which revision and chunks supported an answer, and another agent, Senpi/pi, Codex, Claude Code, or a fresh Harness session, can resume against the same evidence. The "Session continuity in 1.0" section below describes that architecture precisely.

The developer goal is simple:

> **Start with text, get hybrid retrieval, and keep every published knowledge-base revision reproducible.**

OpenRouter embeddings are the default. You do not need a separate embedding pipeline for the normal workflow.

## 30-second quick start

```bash
cairn init --tenant acme --kb handbook
export OPENROUTER_API_KEY='sk-or-...'
```

Create `chunks.jsonl`; vectors are optional:

```json
{"id":"intro-v1-c0","text":"CAIRN stores immutable retrieval revisions.","metadata":{"section":"intro"}}
{"id":"cold-v1-c0","text":"Cold search uses object-store range reads.","metadata":{"section":"architecture"}}
```

Ingest and query:

```bash
cairn ingest chunks.jsonl --dev-calibration
cairn query "how does cold search work?"
```

`ingest` is the friendly alias for `snapshot`; `query` is the alias for `search`. The ingest command reads the complete live corpus once, embeds missing vectors with OpenRouter `search_document`, builds immutable shards and global BM25 statistics, then atomically publishes a revision. Query-time text is embedded with `search_query` using the model/dimension pinned in that revision.

### 한국어 빠른 시작

```bash
cairn init --tenant myteam --kb docs
export OPENROUTER_API_KEY='sk-or-...'
cairn ingest chunks.jsonl --dev-calibration
cairn doctor --full
cairn query "이 문서에서 캐시 정책은 어떻게 동작하나요?"
```

개발할 때는 보통 `init → ingest → query` 세 명령만 알면 됩니다. `build`, `stats`, `publish`는 CI나 고급 delta workflow가 필요할 때만 사용하세요.

---

## DeepSeek Harness in five minutes

CAIRN 1.0 includes a native, precompiled DeepSeek Harness bundle. The model gets one read-only retrieval tool while URL, credentials, tenant/KB, historical revision, and infrastructure budgets remain trusted configuration.

```bash
# Build a revision first.
cairn init --tenant acme --kb handbook
export OPENROUTER_API_KEY='sk-or-...'
cairn ingest chunks.jsonl --dev-calibration

# Start an authenticated, one-scope sidecar.
export CAIRN_SERVER_TOKEN="$(openssl rand -hex 32)"
cairn serve --restrict-to-default-scope

# In another shell, install and verify the bundle.
export CAIRN_SERVER_TOKEN='the-same-value'
./scripts/install_dsh_bundle.sh web
dsh --profile web web
```

The agent can now call `cairn_search`. Canonical results contain immutable revision, embedding, corpus-digest, score, and `cairn://` citation provenance. See [DEEPSEEK_HARNESS.md](DEEPSEEK_HARNESS.md) for remote profiles, multiple KBs, fixed authorization filters, and the complete security contract.

## Session continuity in 1.0

This is the headline architecture CAIRN 1.0 delivers, and it is worth being precise about what that means.

**What this repository ships in 1.0:** the retrieval primitives plus the continuity tooling built on them.

- Immutable, content-addressed revisions. A published revision can never silently change under a citation.
- Revision provenance in every result: revision number, embedding provider/model/dimension, canonical live-corpus SHA-256, cold/warm mode, and calibrated score domain.
- Stable `cairn://` citations, for example `cairn://acme/handbook/revision/42/chunk/doc-v3-c7`, recorded in DeepSeek Harness session logs as durable structured tool results.
- Explicit HEAD/rollback semantics and a revision lineage chain (`parent_revision`), so "the same knowledge base as yesterday" is a checkable fact, not a hope.
- `scripts/session_import.py`, a Python-stdlib importer that reads DeepSeek Harness, Senpi/pi, Codex, and Claude Code session stores; normalizes them into redacted chunks; and emits CAIRN-ready JSONL with deterministic, resumable identities.
- A second DeepSeek Harness mount (`cairn_search_sessions`) plus the `cairn_session_packet` tool, which turns an exact session into a bounded, sequence-ordered continuation packet.
- A same-origin web console for scope health, hybrid search, imported session lookup, continuation packets, and server-owned query history that can survive a web-process restart.

**The 1.0 continuity model:** a session's retrieval history is its memory. Because each result carries its revision and corpus digest, a continuation works like this:

1. The originating session (DeepSeek Harness, Senpi/pi, Codex, or Claude Code) is imported with `scripts/session_import.py` into a dedicated CAIRN sessions knowledge base.
2. A new session searches that KB — or calls `cairn_session_packet` with an exact `session_uid` — and pins the recorded revision and corpus SHA-256.
3. If HEAD has advanced, the revision comparison surfaces drift while the recorded corpus digest identifies the exact corpus the prior answer used. Citations remain valid as identifiers; replaying against the pinned revision requires explicitly enabling historical revision access on the server, and an old revision can predate a deletion, so drift is surfaced rather than hidden.

DeepSeek Harness's own session store (`~/.dsh/sessions/**/session.jsonl.zstd` plus the `session_projcache.json` and `workspace.json` indexes) is a first-class input — a fresh Harness session can search and continue its own history, not just other agents'. See [SESSION_CONTINUITY.md](SESSION_CONTINUITY.md) for the importer pipeline, privacy tiers, and checkpoint semantics, and [examples/deepseek-harness.sessions.patch.yml](examples/deepseek-harness.sessions.patch.yml) for the mount.

## Why CAIRN exists

A conventional RAG stack often separates source documents, BM25, vectors, metadata, and revision state. Rollback and reproducibility then become harder than the retrieval math itself.

CAIRN treats a knowledge-base revision as one immutable search snapshot:

```text
revision
├── embedding provider / model / dimension
├── immutable cold-search shards
├── revision-global BM25 statistics
├── tombstones
├── calibration parameters
├── optional UQA warm database
└── manifest + commit marker
```

The object store is the durable source of truth. Local disk and RAM are caches.

```text
                    Object Store (R2/local)
                             │
                    immutable revision
                             │
             ┌───────────────┴───────────────┐
             ▼                               ▼
        cold retrieval                  warm retrieval
     BM25 + IVF/INT8                    local UQA-RS
       + FP16 rerank                    GIN / HNSW
             └───────────────┬───────────────┘
                             ▼
                    calibrated evidence
                             │
                    one revision prior
                             ▼
                         final top-k
```

## Installation

Core-only:

```bash
cargo build --release --no-default-features --bin cairn
```

For optional UQA warm execution, place this directory at `uqa-rs/integrations/cairn` and run:

```bash
cargo build --release --manifest-path Cargo.uqa.toml --features uqa --bin cairn
```

Read [UQA_COMPATIBILITY.md](UQA_COMPATIBILITY.md) before enabling UQA. It remains opt-in because it has its own licensing boundary.

Developer checks:

```bash
make check
make check-uqa   # only when developing with UQA
```

See [VALIDATION.md](VALIDATION.md) for the exact release gate.

---

## Input JSONL

One line is one immutable chunk.

```json
{"id":"doc-42-v3-c0","text":"The refund window is 30 days."}
{"id":"doc-42-v3-c1","text":"Enterprise plans include audit logs.","metadata":{"plan":"enterprise","lang":"en"}}
```

Precomputed vectors are also accepted:

```json
{"id":"doc-42-v3-c2","text":"Pre-embedded text","vector":[0.12,-0.03,0.91]}
```

Chunk IDs are immutable identities. Prefer `doc42-v1-c0 → tombstone` followed by `doc42-v2-c0`; do not silently change the meaning of the same ID in a later delta.

## OpenRouter embeddings

Default configuration:

```toml
[embedding]
provider = "openrouter"
model = "qwen/qwen3-embedding-8b"
dimensions = 1024
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
batch_size = 32
max_batch_bytes = 2097152
concurrency = 4
timeout_secs = 60
```

Pre-embed a reusable corpus:

```bash
cairn embed raw.jsonl -o embedded.jsonl
cairn embed raw.jsonl -o embedded.jsonl --missing-only
```

To guarantee no embedding API call:

```bash
cairn ingest embedded.jsonl \
  --no-embed \
  --embedding-provider external \
  --embedding-model my/internal-model \
  --dev-calibration
```

For an `external` revision, CAIRN cannot invent a compatible query vector. Provide `--vector-file` or intentionally use `--lexical-only`; it never silently downgrades normal hybrid search.

---

## Configuration

`cairn init` creates `.cairn/config.toml`. Relative paths are resolved relative to that config file, not the shell's current directory. Secrets are never written to the file; only environment-variable names are stored.

```toml
[store]
local = "store"
bearer_env = "CAIRN_BEARER_TOKEN"

[embedding]
provider = "openrouter"
model = "qwen/qwen3-embedding-8b"
dimensions = 1024
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
batch_size = 32
max_batch_bytes = 2097152
concurrency = 4
timeout_secs = 60

[defaults]
tenant = "acme"
knowledge_base = "handbook"
cache = "cache"
```

**CAIRN rejects unknown configuration fields.** A typo such as `dimensons = 1024` fails immediately instead of being ignored.

Inspect the effective, non-secret configuration:

```bash
cairn config
cairn --format json config | jq
```

Useful environment variables:

```text
CAIRN_CONFIG
CAIRN_TENANT
CAIRN_KB
CAIRN_BEARER_TOKEN
OPENROUTER_API_KEY
RUST_LOG
```

## Doctor: diagnose before debugging

```bash
cairn doctor                   # free: config + read connectivity
cairn doctor --check-write     # create/head/delete probe
cairn doctor --check-embedding # one tiny live OpenRouter request
cairn doctor --full            # both paid/effectful checks
```

Plain `doctor` deliberately does not spend embedding credits.

## CLI map

```text
Happy path:
  cairn init             create project config
  cairn ingest           full corpus -> embed/build/publish
  cairn query            hybrid search

Operations:
  cairn config           show effective non-secret config
  cairn doctor           diagnose setup
  cairn head             show current revision
  cairn promote          move HEAD to committed revision
  cairn rollback         alias for promote
  cairn serve            HTTP server

Artifacts / advanced pipelines:
  cairn embed            pre-embed JSONL
  cairn snapshot         canonical name for ingest
  cairn search           canonical name for query
  cairn build            build immutable cold shards only
  cairn stats            build revision-global stats only
  cairn compact          compact canonical corpus
  cairn publish          advanced/delta publication
  cairn uqa-build        build warm UQA DB
  cairn inspect-shard    validate shard metadata
  cairn completions      shell completion
```

Help is available at every level:

```bash
cairn --help
cairn ingest --help
cairn query --help
```

Machine-readable output:

```bash
cairn --format json head
cairn --format json query "retrieval" | jq '.hits[] | {id, score}'
```

Shell completion:

```bash
cairn completions zsh > ~/.zfunc/_cairn
cairn completions fish > ~/.config/fish/completions/cairn.fish
```

## Search examples

```bash
# normal hybrid search
cairn query "how are revisions committed?"

# metadata filters
cairn query "retention" --filter lang=ko --filter plan=enterprise

# long query from a file or stdin
cairn query --query-file question.txt
cat question.txt | cairn query --query-file -

# explicit vector
cairn query "policy" --vector-file query-vector.json

# lexical-only debugging
cairn query "exact identifier" --lexical-only
```

## Revision model

Publishing is append-only; `HEAD` is the mutable pointer.

```text
revision 1  committed
revision 2  committed
revision 3  committed  <- HEAD
```

Rollback is explicit:

```bash
cairn rollback 2 --dry-run
cairn rollback 2
```

The next ingest does **not** reuse revision 3. CAIRN selects the next unused revision number, preserving immutable revision identity.

Historical HTTP search is disabled by default because an old revision can predate a deletion. Enable it only when you have a deliberate continuity or audit use case, and treat it as a privileged operation.

## Full snapshots vs advanced deltas

Prefer full snapshots until corpus size makes them impractical:

```bash
cairn ingest complete-live-corpus.jsonl --shards 4 --dev-calibration
```

This gives the simplest mental model and exact live-corpus BM25 statistics.

For advanced pipelines:

```bash
cairn build embedded.jsonl --out build/shards --shards 8
cairn stats complete-live-corpus.jsonl --out build/stats.json --scoring scoring.json
cairn publish --help
```

`build`, `stats`, `compact`, and `uqa-build` are intentionally deterministic/network-free artifact commands. Publication is a separate effectful step.

---

## HTTP server

```bash
cairn serve --listen 127.0.0.1:8080
```

Health and version:

```bash
curl http://127.0.0.1:8080/health
curl http://127.0.0.1:8080/version
```

Search:

```bash
curl -sS \
  -H 'content-type: application/json' \
  -d '{"query":"object storage","limit":10}' \
  http://127.0.0.1:8080/v1/acme/kb/handbook/search | jq
```

Unknown request fields are rejected, catching client typos early. The server does not require `OPENROUTER_API_KEY` merely to boot: explicit-vector and lexical-only requests still work. A request that actually needs automatic embedding fails with an actionable error when the key is absent.

`/health` and `/version` are public. `/head` and `/search` use bearer authentication when `CAIRN_SERVER_TOKEN` is set, and return structured errors with a machine-readable `code`, a `retryable` flag, and a `request_id` for log correlation.

## R2 / Cloudflare

The included gateway is under `cloudflare/r2-gateway`.

```text
R2             durable source of truth
Container disk disposable warm cache
RAM            metadata/query cache
```

Use HTTPS in production. Plain HTTP service URLs are accepted only for exact loopback hosts (`localhost`, `127.0.0.1`, `::1`). Redirects are disabled for credential-bearing HTTP clients.

## Retrieval semantics

Cold retrieval combines BM25 postings, IVF candidate generation over quantized vectors, FP16 exact reranking, revision-specific calibration, and one global corpus prior.

Warm UQA and cold CAIRN raw scores are **not assumed to be numerically equivalent**. Each has separate calibration parameters and maps into the common `revision_calibrated_log_odds` domain before fusion.

`--dev-calibration` is for development only. Production ranking should use held-out relevance labels and validated calibration.

## Functional Rust design

CAIRN follows **functional core / imperative shell** rather than forcing every operation into iterator-heavy code.

Mostly pure core:

- BM25/tokenization;
- vector normalization, quantization, scoring;
- calibration and evidence fusion;
- manifest/descriptor validation;
- query-planning bounds;
- response validation.

Effectful shell:

- CLI orchestration;
- OpenRouter HTTP;
- R2/local storage;
- revision CAS;
- cache lifecycle;
- shard construction;
- UQA execution.

Project rules include `#![forbid(unsafe_code)]`, checked external size conversions, bounded remote I/O/concurrency, failure-atomic file replacement, immutable IDs + tombstones, and opt-in UQA integration.

The query-embedding cache is bounded by an approximate memory budget as well as entry count, preventing very high-dimensional embeddings from turning a harmless entry cap into excessive RAM usage.

## What's new in 1.0

1.0 is the first public release, framed around the DeepSeek Harness ecosystem:

1. installable `cairn-uqa-dsh` Cordis bundle with canonical typed results;
2. cooperative Harness cancellation and tool-call correlation;
3. bearer-authenticated `/head` and `/search` plus structured API errors;
4. optional one-tenant/KB server scope fence;
5. immutable embedding/corpus provenance in search results and `cairn://` citations;
6. bounded retries, response acquisition, model context, and metadata;
7. explicit untrusted-evidence rendering and fixed deployment filters;
8. the cross-agent session continuity architecture described above, delivered on revisioned citations and corpus digests.

See [REVIEW.md](REVIEW.md) for the detailed engineering review.

## Core invariants

```text
object exists          != committed revision
committed revision     = immutable publication state
HEAD                    = mutable pointer only
local cache             = disposable
chunk ID                = immutable identity
deletion                = tombstone + eventual compaction
cold/warm raw scores    = different domains
final score             = common calibrated domain
historical read         = privileged operation
```

A failed publication may leave content-addressed orphan objects, but it must not make the revision visible.

## Troubleshooting

### `OPENROUTER_API_KEY is not set`

```bash
export OPENROUTER_API_KEY='sk-or-...'
```

Or use precomputed vectors / lexical-only mode.

### `revision uses external embeddings`

Provide the compatible query vector:

```bash
cairn query "..." --vector-file query-vector.json
```

or intentionally request lexical-only search.

### `stale parent` during ingest

Another writer published while your embedding/index build was running. This is expected fail-closed behavior. Refresh HEAD and rerun the ingest.

### Search is unexpectedly approximate

IVF/vector retrieval and post-retrieval metadata filtering can be approximate. Increase the candidate pool or use a filter-aware strategy for highly selective filters.

### Config fails after upgrading

CAIRN rejects unknown fields. Run:

```bash
cairn config
```

and fix the field named in the parse error. This is deliberate: configuration typos should fail early.

## Security notes

- Keep API keys in environment variables or a secret manager.
- Do not put bearer tokens into config files or command-line arguments.
- Put public `cairn serve` behind authentication/authorization.
- Historical revision access deserves separate authorization.
- Use HTTPS for non-loopback OpenRouter/object-gateway endpoints.
- CAIRN verifies content-addressed artifacts and bounded decompression/decoding before use.
- Retrieved documents are data, not agent instructions. The Harness bundle renders evidence inside an explicit untrusted-evidence boundary; keep that boundary intact in any custom integration.

## Limitations

- Cold lexical retrieval is accumulator-based, not BMW/WAND yet.
- Metadata filtering is post-retrieval and can require a wider candidate pool.
- ANN retrieval is approximate by design.
- Warm UQA requires an explicit compatible UQA-RS checkout.
- Production calibration requires your own labeled relevance data.
- The session importer ships as a Python-stdlib ETL with metadata-first and transcript opt-in tiers; DeepSeek Harness's pre-release session format may still change upstream and is version-pinned at import time.

## Repository map

```text
src/
  embedding.rs        OpenRouter boundary
  manifest.rs         revisions / HEAD / commit markers
  object_store.rs     local + HTTP object stores
  runtime.rs          cold/warm orchestration
  search/             retrieval and fusion
  index/              immutable shard format/build/read
  cache.rs            disposable warm cache
  uqa.rs              optional UQA adapter
  bin/cairn/          single developer CLI
integrations/deepseek-harness/  native DSH bundle (source, ESM, tests)
web/                   same-origin browser console and BFF
cloudflare/r2-gateway/ R2 gateway
examples/              sample config/corpus/scoring/profile patch
tests/                 integration and CLI tests
scripts/               validation/install helpers
```

## License

CAIRN is Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Optional UQA-RS integration is a separate dependency/license boundary; see [UQA_COMPATIBILITY.md](UQA_COMPATIBILITY.md).
