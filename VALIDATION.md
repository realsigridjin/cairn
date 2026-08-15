# Validation status — CAIRN 2.1.0

Executed in this environment:

```text
DeepSeek plugin strict offline TypeScript emit: PASS
DeepSeek plugin Node tests: 7/7 PASS
DeepSeek plugin npm pack dry-run: PASS
CAIRN dependency-free structural validation: PASS
TOML / JSON / JSONL / YAML parsing: PASS
shell syntax checks: PASS
source SHA-256 manifest verification: PASS
plugin tgz checksum/tar listing: PASS
v2.0 -> v2.1 patch round-trip: PASS
final ZIP CRC/integrity: PASS
```

Plugin tests cover unsafe URL rejection, CAIRN version/API compatibility,
bearer/call-id propagation, retryable and non-retryable failures, response
bounds/content type, strict provenance decoding, duplicate rejection, metadata
allowlisting, native tool registration/execution, unknown-argument rejection,
revisioned citations, and untrusted-evidence rendering.

Not executed here:

- `cargo fmt`, `cargo test`, `cargo clippy` — no cargo/rustc/rustfmt installed;
- UQA feature compilation — requires the exact external UQA-RS checkout;
- normal npm typecheck against real Harness packages — registry access is
  unavailable and this host's Node 22.16 is below Harness's declared 22.19 floor;
- a real DeepSeek Harness profile boot/session-log test;
- real R2/OpenRouter production credentials and representative relevance/load
  benchmarks.

Required release gate:

```bash
make check
make check-uqa

# On Node ^22.19 or >=24 with the exact pinned Harness dependencies:
cd integrations/deepseek-harness
npm install
npm run check

dsh plugin --profile smoke add ../../dist/cairn-uqa-dsh-2.1.0.tgz
dsh --profile smoke --dump-config
cairn-dsh-doctor --query "release smoke test"
```

Do not describe a deployment as production-ready until those gates and
representative recall/calibration/cold-latency/failure-injection benchmarks pass.
