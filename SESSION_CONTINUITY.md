# CAIRN Session Continuity

CAIRN 1.0 treats prior coding-agent sessions as first-class retrieval evidence. The goal is not to replace DeepSeek Harness sessions; it is to make their useful context portable, searchable, and safe to hand to a new agent run.

The continuity layer supports local history from:

- DeepSeek Harness (`~/.dsh`, or `$DSH_HOME`)
- Senpi and pi (`~/.senpi`, `~/.pi`)
- Codex (`~/.codex`, or `$CODEX_HOME`)
- Claude Code (`~/.claude`)

## Design boundary

Harness session formats change independently of CAIRN's immutable retrieval format. CAIRN therefore keeps volatile parsing out of the Rust retrieval core:

```text
local harness stores
  -> scripts/session_import.py
  -> normalized, redacted session chunks
  -> cairn embed / cairn ingest
  -> dedicated CAIRN sessions knowledge base
  -> DeepSeek Harness retrieval mount
  -> cairn_session_packet continuation tool
  -> same-origin CAIRN web BFF
```

The retrieval core, manifest format, revision model, and object-store protocol remain unchanged.

## Privacy model

Import is metadata-first. The default tier imports identity, lineage, timestamps, working directory, model, usage, counts, and redacted titles or prompt previews. Transcript import is opt-in.

The importer never reads credential stores or environment files, including:

- `~/.dsh/.credentials.yaml`
- `~/.dsh/settings.yaml`
- `~/.codex/auth.json`
- `~/.senpi/agent/auth.json`
- `~/.claude.json`
- `.env` files

Before text is indexed, the redaction pass removes common API keys, bearer tokens, JWTs, private-key blocks, and secret assignments. Retrieved session text remains untrusted evidence; it is never instructions.

## Identity and incremental import

Every imported session has a deterministic identity:

```text
<source>:<native-session-id>
```

Sessions emit immutable chunk identities:

```text
s:<source>:<native-id>:meta
s:<source>:<native-id>:m:<start>-<end>
```

A re-run over unchanged stores is byte-for-byte idempotent. Appended JSONL sessions resume from a checkpoint; changed compressed DeepSeek Harness sessions are decoded again because zstd frames are not safely seekable. Checkpoints are written only after the canonical corpus is written successfully.

## Import

Discover supported local stores without writing state:

```bash
python3 scripts/session_import.py --discover
```

Build the canonical metadata-tier corpus:

```bash
python3 scripts/session_import.py export \
  --out .cairn/session-chunks.jsonl
```

Opt into redacted transcript windows:

```bash
python3 scripts/session_import.py export \
  --tier transcript \
  --out .cairn/session-chunks.jsonl
```

Publish through the normal CAIRN pipeline:

```bash
cairn ingest .cairn/session-chunks.jsonl \
  --tenant local \
  --kb sessions \
  --dev-calibration
```

For large histories, keep embeddings in the canonical corpus and run `cairn embed --missing-only` before ingest so unchanged chunks do not pay for another embedding request.

## DeepSeek Harness use

Mount the standard CAIRN retrieval bundle twice: once for project knowledge and once for imported sessions. The sessions mount uses a unique tool name and session metadata allowlist:

```yaml
- id: cairn-session-memory
  config:
    baseUrl: http://127.0.0.1:8080
    tenant: local
    knowledgeBase: sessions
    toolName: cairn_search_sessions
    tokenEnv: CAIRN_SERVER_TOKEN
    metadataKeys:
      - doc_type
      - source
      - session_uid
      - root_session_id
      - parent_id
      - cwd
      - repo_path
      - git_branch
      - model
      - created_at_ms
```

The `cairn_session_packet` tool takes a `session_uid`, fetches its exact session chunks, sorts windows by sequence rather than retrieval score, and renders a bounded continuation packet with revision, corpus digest, lineage, citations, and untrusted-evidence boundaries.

## Web use

The web application uses a same-origin backend-for-frontend. The browser never receives `CAIRN_SERVER_TOKEN`; the BFF holds it server-side and exposes only bounded endpoints for health, scope, search, session inspection, and continuation packets.

A typical local launch is:

```bash
CAIRN_BASE_URL=http://127.0.0.1:8080 \
CAIRN_SERVER_TOKEN=... \
CAIRN_WEB_SCOPES=acme/handbook,local/sessions \
CAIRN_WEB_SESSIONS_SCOPE=local/sessions \
CAIRN_WEB_HISTORY_PATH=.cairn/web-history.jsonl \
npm --prefix web start
```

`CAIRN_WEB_SESSIONS_SCOPE` must also appear in `CAIRN_WEB_SCOPES`. This is deliberate: a continuation packet can read only the explicitly configured sessions KB, never an arbitrary tenant/KB passed by the browser.

The browser-facing routes are:

- `/sessions` — separates imported CAIRN session memory from the console's own query history.
- `/sessions/packet?uid=<session_uid>` — renders a bounded continuation packet for an exact session.
- `/api/sessions/packet?uid=<session_uid>` — the same packet as JSON for tools and tests.

`CAIRN_WEB_HISTORY_PATH` is optional. When set, server-owned query history is stored as bounded JSONL, recovers after a restart, skips a truncated crash tail, and uses fsync-plus-rename for compaction. The bearer token is not part of a history record and never reaches that file. Without the variable, history remains in memory only.

The session packet view shows source, working directory, lineage, model, timestamps, usage, and ordered message windows when transcript-tier chunks were imported. A metadata-tier import still shows lineage and revision state, and says plainly that ordered transcript evidence is unavailable until transcript import is enabled. The handoff view compares the packet's pinned revision with current HEAD, carries the corpus digest as provenance, and reports exact, advanced, or incompatible state.

## Failure behavior

- Malformed one-file records are reported without discarding unrelated stores.
- Unknown DeepSeek Harness session format versions stop that store rather than guessing.
- Duplicate imports converge to the same canonical chunk set.
- Missing or stale checkpoints trigger re-import and convergence, not silent loss.
- Secrets found in imported text are replaced before ingest.
- Historical replay remains disabled unless the CAIRN server is explicitly started with historical revision access.

## Verification surfaces

The release evidence for this feature is not a parser unit test alone. It includes synthetic per-harness fixtures, malformed/duplicate/incremental/privacy tests, a local CAIRN ingest and search, the DeepSeek packet tool tests, and a real browser workflow against the web BFF.
