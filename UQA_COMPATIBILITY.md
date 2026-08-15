# UQA-RS integration boundary — CAIRN 2.1.0

Install CAIRN at `uqa-rs/integrations/cairn`. Optional path dependencies point to
`../../crates/uqa-engine` and `../../crates/uqa-core`; default features remain
empty. Cold retrieval, revisions, HTTP, CLI, and the DeepSeek Harness plugin do
not require UQA.

The warm adapter expects the reviewed UQA surface: `Engine::open`, SQL execution
and parameters, lexical `text_match`, vector `knn_match`, analyzer/index setup,
and scalar/vector values used by `src/uqa.rs`. Pin the exact UQA commit and run:

```bash
cargo test --all-targets --features uqa
cargo clippy --all-targets --features uqa -- -D warnings
```

CAIRN owns durable revision lineage, object layout, cold search, global stats,
tombstones, calibration/fusion, activation, and cache policy. UQA-RS is only an
optional local warm execution kernel. The DeepSeek Harness bundle talks to the
CAIRN HTTP API and does not link UQA-RS.

CAIRN is Apache-2.0. UQA-RS is external and retains its own exact revision and
license terms; enabling/linking UQA does not relicense it.
