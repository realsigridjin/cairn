# Validation status — CAIRN 1.0.0

Executed in this environment:

```text
Rust formatting gate: PASS
Rust standalone tests (--all-targets --no-default-features): PASS
Rust standalone clippy (-D warnings): PASS
DeepSeek plugin strict offline TypeScript emit: PASS
DeepSeek plugin Node tests: 13/13 PASS
DeepSeek plugin npm pack dry-run: PASS
Session importer unittest suite: 67/67 PASS
Web strict TypeScript typecheck: PASS
Web Node tests: 183/183 PASS
CAIRN dependency-free structural validation: PASS
TOML / JSON / JSONL / YAML parsing: PASS
shell syntax checks: PASS
source SHA-256 manifest verification: PASS
plugin tgz checksum/tar listing: PASS
packaged source integrity round-trip: PASS
final ZIP CRC/integrity: PASS
```

Session-continuity evidence executed against real local stores and a live CAIRN server:

```text
DeepSeek Harness sessions discovered/imported: 1
Senpi sessions discovered/imported: 341
Codex sessions discovered/imported: 495
Claude Code sessions discovered/imported: 391
pi local session store: absent on this host
canonical metadata chunks emitted: 1,228
published scope: local/sessions revision 1
live DeepSeek Harness session search: PASS
live Senpi session search: PASS
live Codex session search: PASS
live Claude Code session search: PASS
live exact-session continuation packet: PASS
live web history restart recovery: PASS
browser viewport QA at 1440px and 390px: PASS, no horizontal overflow
browser output token-exposure check: PASS
```

Plugin tests cover unsafe URL rejection, CAIRN version/API compatibility,
bearer/call-id propagation, retryable and non-retryable failures, response
bounds/content type, strict provenance decoding, duplicate rejection, metadata
allowlisting, native tool registration/execution, unknown-argument rejection,
revisioned citations, and untrusted-evidence rendering.

Web tests cover scope fencing, token confidentiality, response decoding,
bounded history, durable JSONL recovery, malformed history repair, retention,
mobile overflow regressions, session-packet ordering, lineage extraction,
metadata-tier sessions, drift classification, and packet error bounds.

Not executed here:

- UQA feature compilation/tests — requires the exact external UQA-RS checkout;
- normal npm typecheck against real Harness packages — registry access is
  unavailable in this environment;
- a real DeepSeek Harness profile boot/session-log test;
- real R2/OpenRouter production credentials and representative relevance/load
  benchmarks.

Required release gate:

```bash
make check
python3 -m unittest discover -s scripts/cairn_sessions/tests
npm --prefix web run check
cd integrations/deepseek-harness
npm run check

# On a host with the real DeepSeek Harness CLI:
dsh plugin --profile smoke add ../../dist/cairn-uqa-dsh-1.0.0.tgz
dsh --profile smoke --dump-config
cairn-dsh-doctor --query "release smoke test"
```

Do not describe a deployment as production-ready until those gates and
representative recall/calibration/cold-latency/failure-injection benchmarks pass.
