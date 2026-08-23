# CAIRN

**Revisioned retrieval for project knowledge and past coding-agent work.**

CAIRN turns a corpus or local coding-session history into a searchable,
immutable knowledge base. Every result is pinned to a revision and carries a
`cairn://` citation, so an agent can show exactly what evidence it used and
which corpus state produced it.

- **Project knowledge** — hybrid, lexical, or vector retrieval over JSONL
  corpora.
- **Session continuity** — searchable history from Senpi/pi, Codex, Claude
  Code, and compatible local JSONL stores.
- **Reproducibility** — immutable shards, revision-pinned citations, corpus
  digests, embedding provenance, and calibrated retrieval metadata.
- **Version** 1.0.0 · **API** v1 · **License** Apache-2.0

## Why CAIRN

CAIRN's value is retrieval quality under the conditions coding agents actually
face: vague recall questions over large, noisy, and occasionally deleted
session transcripts.

An additive benchmark compared CAIRN revision 8 against a deterministic
`grep`-only baseline on the same **1,489 raw-transcript-file** import and
**12 positive recall cases**. At benchmark time grep could read the 1,483
surviving local files (3.29 GB); six files deleted after import remained
searchable in CAIRN but were deliberate grep misses. The default candidate is
**H1**: one hybrid query, rather than lexical-only or five-query expansion.

| Metric | grep-only G1 | CAIRN hybrid H1 |
| --- | ---: | ---: |
| nDCG@10 | 0.110 | **0.754** |
| Recall@10 | 0.120 | **0.882** |
| MRR@10 | 0.153 | **0.804** |
| Relevant session reached within 10 candidates | 3 / 12 | **12 / 12** |
| Paired nDCG@10 difference | — | **+0.644, exact p = 0.00049** |

The comparison uses the same frozen qrels, metric code, seed, and bootstrap
settings for both arms. It does **not** claim a universal answer-quality win:
the separate one-shot downstream answer test did not show an answer-quality
gain. The measured claim is narrower and useful: hybrid CAIRN finds relevant
prior work much more reliably than searching raw transcripts with grep.

### Token and API cost

Retrieval itself makes no runtime LLM call. H1 embeds its query with
Qwen3-Embedding-8B at a measured average of **24.1 embedding tokens** and
**$0.000000241 per query**. For the revision-8 corpus, one-time vectorization
of 25,885 chunks (226.5 MB text) is estimated at **$0.57–$1.36**, using
1.67–4.0 characters per token for the mixed Korean/code corpus.

LLM context is a separate cost: an agent still has to read the evidence that a
tool returns. A reproducible rank-walk model sums text until it reaches the
first relevant session, or gives up after ten candidates. It converts
characters to a token range with 4.0 chars/token for English/code-heavy text
and 1.67 chars/token for Korean-heavy text.

| Retrieval path | Relevant within 10 | Mean evidence read per case | Estimated LLM input tokens |
| --- | ---: | ---: | ---: |
| grep G1, 50,000-character cap per candidate | 3 / 12 | 400,000 chars | 100.0K–239.5K |
| **CAIRN H1** | **12 / 12** | **76,796 chars** | **19.2K–46.0K** |

Under that deliberately bounded grep model, H1 saves **323,204 characters**
per recall case — about **80.8K–193.5K LLM input tokens**, or **5.21× less
context** — while finding four times as many cases. Without the per-candidate
cap, common grep terms expand to 339.8M characters per case on this corpus;
that is an upper-bound stress result, not a practical prompt size.

The downstream answer prompt is separately bounded to a 5,000-character memory
packet (about 1.25K–2.99K input tokens). These figures are context-size
estimates rather than a specific provider's tokenizer telemetry; add your
model's input-token price to price the final answer call.

## Quick start

Build CAIRN and create a local, fully offline demonstration knowledge base:

```bash
cargo build --release --no-default-features --bin cairn
export PATH="$PWD/target/release:$PATH"

cairn init --tenant acme --kb handbook
cairn snapshot examples/chunks.jsonl \
  --no-embed \
  --embedding-provider external \
  --embedding-model offline-demo \
  --dev-calibration

cairn search "cold search" --tenant acme --kb handbook --lexical-only
```

`snapshot` atomically publishes a complete live corpus; it is a full
replacement, not a delta update. Use `cairn doctor` before connecting a real
embedding provider or object gateway.

To serve a knowledge base over HTTP:

```bash
export CAIRN_SERVER_TOKEN="$(openssl rand -hex 32)"
cairn serve --listen 127.0.0.1:18080
```

Search responses report execution mode (`cold` or `warm`), retrieval mode
(`lexical`, `hybrid`, or `vector`), calibrated score, embedding provenance,
corpus digest, and revision-pinned `cairn://` citations.

## Index past coding-agent work

The importer is read-only against local session stores. It redacts secrets
before writing normalized JSONL and defaults to the privacy-safe metadata tier.
Use `--tier transcript` only when message windows are needed for recall.

```bash
# Discover supported local session stores.
python3 scripts/session_import.py --discover --json

# Import in the background; this is safe to resume.
mkdir -p .cairn/agent-run
nohup python3 scripts/session_import.py \
  --out .cairn/session-chunks.jsonl \
  --checkpoint .cairn/session-import-checkpoint.json \
  --tier transcript \
  --json > .cairn/agent-run/session-import.log 2>&1 &

# Embed and publish a new immutable sessions revision.
cairn embed .cairn/session-chunks.jsonl \
  --out .cairn/session-chunks-embedded.jsonl \
  --missing-only
cairn snapshot .cairn/session-chunks-embedded.jsonl \
  --tenant local \
  --kb sessions \
  --dev-calibration

cairn search "find the session where we debugged deployment" \
  --tenant local \
  --kb sessions
```

Imported sessions are **untrusted evidence**, never instructions. CAIRN keeps
their lineage and sequence order so an agent can retrieve a concise,
revision-safe continuation packet instead of reopening raw transcript files.
See [SESSION_CONTINUITY.md](SESSION_CONTINUITY.md) for the schema, privacy
boundary, packet contract, and revision-drift behavior.

## Web console

The same-origin browser console in `web/` keeps the bearer token server-side
and shows revision, corpus digest, and citations beside every result.

| Connection health | Hybrid search workbench |
| --- | --- |
| ![Connection health](docs/screenshots/web-connect.png) | ![Hybrid search workbench](docs/screenshots/web-search.png) |

| Session memory and query history | Continuation packet |
| --- | --- |
| ![Session memory](docs/screenshots/web-sessions.png) | ![Continuation packet](docs/screenshots/web-session-packet.png) |

```bash
CAIRN_BASE_URL=http://127.0.0.1:18080 \
CAIRN_SERVER_TOKEN=$CAIRN_SERVER_TOKEN \
CAIRN_WEB_SCOPES=acme/handbook,local/sessions \
CAIRN_WEB_SESSIONS_SCOPE=local/sessions \
CAIRN_WEB_PORT=18787 \
npm --prefix web start
```

## Optional warm execution

CAIRN can run inside a UQA-RS checkout for persistent warm indexes and faster
repeated queries. This is opt-in: the standalone build has no UQA dependency.
See [UQA_COMPATIBILITY.md](UQA_COMPATIBILITY.md) for the supported version
boundary, feature flags, and exact build/test commands.

## Verify a checkout

```bash
make check                                                   # Rust fmt/tests/clippy + static validation
python3 -m unittest discover -s scripts/cairn_sessions/tests # importer suite
npm --prefix web run check                                   # web typecheck + tests
```

## Docs

| File | Contents |
| --- | --- |
| [AGENTS.md](AGENTS.md) | Autonomous setup, background indexing, server runbook, and quality gates |
| [SESSION_CONTINUITY.md](SESSION_CONTINUITY.md) | Session import schema, privacy boundary, and continuation packets |
| [UQA_COMPATIBILITY.md](UQA_COMPATIBILITY.md) | UQA-RS boundary and warm-execution gates |
| [DESIGN.md](DESIGN.md) | Web console design system |

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
