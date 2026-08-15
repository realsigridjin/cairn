#!/bin/sh
set -eu

cargo fmt --all -- --check
cargo test --all-targets --no-default-features
cargo clippy --all-targets --no-default-features -- -D warnings
python3 scripts/static_validate.py

if [ "${CAIRN_CHECK_UQA:-0}" = "1" ]; then
  cargo test --manifest-path Cargo.uqa.toml --all-targets --features uqa
  cargo clippy --manifest-path Cargo.uqa.toml --all-targets --features uqa -- -D warnings
fi
