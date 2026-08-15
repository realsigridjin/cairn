# CAIRN 2.1 — full DeepSeek Harness integration review

This pass reviewed CAIRN 2.0 as a retrieval engine and as an agent-facing
service. Priorities were immutable-revision correctness, Rust effect boundaries,
HTTP security, the DeepSeek Harness native tool contract, and developer setup.

## High-impact issues fixed

1. **No native Harness artifact.** Added a Cordis bundle using
   `ctx.tools.register(defineTool(...))`, precompiled ESM/declarations, an
   installable tgz, tests, doctor, and profile examples.
2. **Search results lacked enough immutable provenance.** HTTP and CLI responses
   now include embedding provider/model/dimension and canonical corpus SHA-256.
3. **KB HTTP routes had no authentication.** Added optional bearer auth, token
   digest retention, `WWW-Authenticate`, and fail-closed non-loopback binding.
4. **One token could address every namespace.** Added an optional one-tenant/KB
   `ServerScope`; `--restrict-to-default-scope` is the sidecar happy path.
5. **Only search existed.** Added authenticated `/head` for revision/model/schema
   contract checks without paying embedding cost.
6. **Errors were unstructured.** Added stable code/message/retryable/request-id
   envelopes and explicit malformed-JSON rejection.
7. **Agent cancellation/correlation was lost.** The native plugin propagates
   `exec.signal` and a bounded `exec.callId` correlation header.
8. **Harness parameter roots are open.** The executor manually rejects unknown
   top-level arguments instead of silently ignoring model mistakes.
9. **Model could have gained infrastructure control.** Tenant, KB, URL, token,
   revision, candidate ceiling, retries, and fixed filters stay trusted config.
10. **Response acquisition and model context were unbounded.** Success/error
    bodies, hit count, metadata, per-hit text, total text, retries, and deadline
    are all bounded.
11. **Redirects could forward credentials.** Redirects are rejected; HTTP is
    allowed only for exact loopback hosts; URL credentials/query/fragment and
    unsafe paths are rejected.
12. **Project `.env` could redirect a bearer token.** The shipped patch contains
    static loopback defaults and no executable environment expressions. Remote
    destinations live in profile-owned config.
13. **Retrieved prompt injection was not labeled.** Model-facing output uses an
    explicit untrusted-evidence boundary and escapes delimiter text.
14. **HTTP status alone was too coarse for retries.** Structured
    `retryable=false` suppresses retry even on 5xx; transport failure remains
    bounded-retryable.
15. **Developer diagnostics stopped before the paid path.** `cairn-dsh-doctor`
    checks health/head for free and supports an explicit `--query` end-to-end
    embedding/search probe.

## Rust design

CAIRN continues to use **functional core / imperative shell**:

```text
mostly pure                         effectful
BM25 / tokenization                 CLI
vector scoring / quantization       HTTP / OpenRouter
calibration / evidence fusion       R2 / local object store
manifest validation                 revision CAS
response validation                 cache / filesystem / UQA
```

`#![forbid(unsafe_code)]`, checked external arithmetic, bounded concurrency,
failure-atomic file replacement, immutable IDs, tombstones, and opt-in UQA are
preserved. Authentication stores only a SHA-256 token digest after startup and
uses a fixed-work digest comparison.

## Intentionally retained boundaries

- UQA-RS is not bundled and remains optional/license-separated.
- Cold lexical retrieval is accumulator-based rather than BMW/WAND.
- Metadata filtering is post-retrieval and can require a wider candidate pool.
- Cold and warm raw scores keep separate calibrations.
- Historical HTTP search remains disabled by default.
- Internal Rust errors are still `anyhow`; public classification is conservative.
- DeepSeek Harness is a preview API: exact release/commit pinning is required.

## Release gate

This environment has Node/TypeScript but no Rust toolchain. The plugin offline
build/tests/package checks were executed. Before production, run the Rust gates
with the exact UQA checkout and the plugin gate against the exact Harness pin.
