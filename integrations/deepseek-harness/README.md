# `cairn-uqa-dsh` 1.0

Native DeepSeek Harness tool bundle for CAIRN. It registers one bounded,
read-only retrieval tool and preserves CAIRN revision provenance in both the
canonical tool result and model-facing citations.

```text
DeepSeek Harness agent
        │ cairn_search({ query, limit?, filters? })
        ▼
cairn-uqa-dsh (Cordis plugin)
        │ authenticated, cancellable, bounded HTTP
        ▼
CAIRN 1.0
        ├─ OpenRouter query embedding
        ├─ cold BM25 + IVF/INT8 + FP16 rerank
        └─ optional warm UQA-RS execution
```

## Quick start

Start CAIRN:

```bash
export OPENROUTER_API_KEY='sk-or-...'
export CAIRN_SERVER_TOKEN="$(openssl rand -hex 32)"

cairn init --tenant acme --kb handbook
cairn ingest chunks.jsonl --dev-calibration
cairn serve --restrict-to-default-scope
```

Install the prepacked bundle from the CAIRN repository root:

```bash
dsh plugin --profile web add ./dist/cairn-uqa-dsh-1.0.0.tgz
dsh --profile web --dump-config
dsh --profile web web
```

The shipped bundle uses safe local defaults:

```yaml
baseUrl: http://127.0.0.1:8080
tenant: acme
knowledgeBase: handbook
toolName: cairn_search
tokenEnv: CAIRN_SERVER_TOKEN
```

Set the same token value in the Harness process environment. The plugin reads
that environment variable for each request, so token rotation does not require
reloading the plugin.

Verify the service contract:

```bash
cairn-dsh-doctor
cairn-dsh-doctor --query "object storage"  # optional paid embedding/search probe
```

## Security contract

The model may choose only:

- a focused query;
- a bounded hit count;
- optional exact metadata filters when enabled.

The model cannot choose:

- server URL or bearer token;
- tenant or knowledge base;
- historical revision;
- remote byte/range-read budgets;
- candidate hard ceiling;
- deployment-owned fixed filters.

`fixedFiltersJson` is merged after model filters, so trusted values win.
`allowModelFilters=false` disables model filters entirely. Retrieved text is
rendered inside an explicit untrusted-evidence boundary and must never be
executed as instructions.

## Profile-owned override

Override the installed row by id in the profile's own `cordis.patch.yml` and
restate the complete config:

```yaml
- id: cairn-uqa-retrieval
  config:
    baseUrl: https://cairn.internal.example
    tenant: acme
    knowledgeBase: runbooks
    toolName: search_runbooks
    tokenEnv: CAIRN_SERVER_TOKEN
    defaultLimit: 6
    maxLimit: 12
    candidateMultiplier: 12
    maxCandidateLimit: 2000
    timeoutMs: 30000
    retries: 1
    maxRetryDelayMs: 2000
    maxResponseBytes: 8388608
    maxTextCharsPerHit: 4000
    maxTotalTextChars: 16000
    maxMetadataBytesPerHit: 8192
    metadataKeys: [title, source, path, url, page, section, lang]
    fixedFiltersJson: '{"visibility":"agent"}'
    allowModelFilters: false
    healthCheckOnLoad: false
```

Do not derive `baseUrl`, `tokenEnv`, or fixed authorization filters from a
project-local `.env`; a credential-bearing plugin must not let an invocation
directory redirect its token to another origin.

## Tool output

The canonical JSON includes:

```text
revision + corpus SHA-256
embedding provider/model/dimension
cold/warm mode + approximate flag
calibrated scores/evidence
stable cairn:// citations
bounded text + allowlisted metadata
remote bytes/range reads
```

The canonical value remains useful from Harness Code Mode. `output.render`
separately creates model-facing evidence text.

## Development

Harness currently requires Node `^22.19.0 || >=24.0.0`.

```bash
npm install
npm run check
```

Without package-registry access, the source archive includes local compatibility
declarations:

```bash
npm run check:offline
```

The offline check is structural; release against the exact pinned Harness
packages and smoke-test a real profile/tool call on every Harness preview
upgrade.
