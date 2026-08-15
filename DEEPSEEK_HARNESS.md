# CAIRN 2.1 + DeepSeek Harness

CAIRN integrates with DeepSeek Harness as a **native Cordis tool bundle**. No
Harness fork, agent-loop patch, shell wrapper, or MCP bridge is required.

```text
DeepSeek Harness
  tools registry / cancellation / session log / Code Mode
                         │
                         ▼
                cairn-uqa-dsh
                         │
                         ▼
CAIRN 2.1 authenticated HTTP API
  revision provenance / OpenRouter embedding / cold search / UQA warm search
```

## Included artifacts

```text
integrations/deepseek-harness/   source, precompiled ESM, declarations, tests
dist/cairn-uqa-dsh-2.1.0.tgz   installable bundle
scripts/install_dsh_bundle.sh   install + dump-config + doctor helper
examples/deepseek-harness.profile.patch.yml
```

## Local setup

```bash
# 1. Build CAIRN in a pinned UQA-RS checkout.
./scripts/install_into_uqa.sh /path/to/uqa-rs
cd /path/to/uqa-rs/integrations/cairn
cargo build --release --no-default-features --bin cairn
export PATH="$PWD/target/release:$PATH"

# 2. Build a KB.
cairn init --tenant acme --kb handbook
export OPENROUTER_API_KEY='sk-or-...'
cairn ingest chunks.jsonl --dev-calibration

# 3. Start a one-scope authenticated sidecar.
export CAIRN_SERVER_TOKEN="$(openssl rand -hex 32)"
cairn serve --restrict-to-default-scope

# 4. Install the native bundle.
./scripts/install_dsh_bundle.sh web

# 5. Start Harness.
dsh --profile web web
```

`--dev-calibration` is only for smoke testing. Production ranking requires
held-out relevance labels and validated calibration.

## Why native instead of a shell command

The native bundle uses the official Harness tool path:

```text
ctx.tools.register(defineTool(...))
```

This preserves:

- parameter validation and a canonical output schema;
- `exec.signal` cancellation for fetch, retry sleep, and body streaming;
- `exec.callId` correlation through `x-dsh-tool-call-id`;
- durable structured tool results in Harness sessions;
- separate `output.render` model context;
- Code Mode access to the same registered tool;
- automatic deregistration with the Cordis plugin lifecycle.

## Trusted configuration versus model arguments

Model arguments:

```json
{
  "query": "Why is R2 the durable source of truth?",
  "limit": 6,
  "filters": { "lang": "en" }
}
```

Trusted deployment configuration:

```text
base URL / token env / tenant / KB
fixed authorization filters
candidate ceiling / timeout / retries
response byte limit / text context limits / metadata allowlist
```

Tenant, KB, URL, token, historical revision, and infrastructure budgets are not
model-facing. The executor also rejects unknown root arguments because the
Harness parameter root is intentionally open for compatibility.

## Authentication and namespace fencing

CAIRN exposes public `/health` and `/version`; `/head` and `/search` use bearer
authentication when `CAIRN_SERVER_TOKEN` is set.

```bash
cairn serve --restrict-to-default-scope
```

is recommended for a Harness sidecar. It binds the server instance to the
configured default tenant/KB as a second authorization fence. Non-loopback
unauthenticated binds fail unless `--allow-unauthenticated` is explicit.

The server returns structured errors:

```json
{
  "error": {
    "code": "EMBEDDING_UNAVAILABLE",
    "message": "...",
    "retryable": false,
    "request_id": "..."
  }
}
```

The plugin honors `retryable=false` even on a 5xx status.

## Revisioned citations

Every result includes immutable retrieval provenance:

```text
revision
embedding provider/model/dimension
canonical live-corpus SHA-256
cold/warm retrieval mode
calibrated score domain
chunk id
```

The plugin emits stable citations:

```text
cairn://acme/handbook/revision/42/chunk/doc-v3-c7
```

A Harness session can therefore record exactly which CAIRN revision and chunks
supported an answer.

## Prompt-injection boundary

Retrieved documents are data, not agent instructions. The model renderer uses:

```text
BEGIN_UNTRUSTED_CAIRN_EVIDENCE
...
END_UNTRUSTED_CAIRN_EVIDENCE
```

and escapes delimiter text found in documents. This is a defense-in-depth label,
not a substitute for normal model/tool policies.

## Remote profile override

The shipped bundle intentionally hardcodes loopback defaults and contains no
`!!js` environment expressions. Override the complete row by id in the
profile-owned patch:

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
    fixedFiltersJson: '{"workspace_id":"acme","visibility":"agent"}'
    allowModelFilters: false
    healthCheckOnLoad: false
```

For multiple KBs, mount multiple plugin instances with unique Cordis ids and
unique tool names. See `examples/deepseek-harness.profile.patch.yml`.

## Doctor and release gates

Free metadata/auth contract:

```bash
cairn-dsh-doctor
```

Optional paid query-embedding/search path:

```bash
cairn-dsh-doctor --query "object storage"
```

Plugin release gate:

```bash
cd integrations/deepseek-harness
npm install
npm run check
```

DeepSeek Harness is a developer preview. Pin the exact Harness version or Git
commit and repeat profile boot, tool invocation, cancellation, and session-log
smoke tests whenever that pin changes.
