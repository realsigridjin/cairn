SHELL := /bin/sh
.PHONY: help fmt test test-uqa clippy clippy-uqa static check check-uqa check-dsh check-all
help:
	@printf '%s\n' \
	  'make check      - format + core tests + clippy + static validation' \
	  'make check-uqa  - check plus UQA feature tests/clippy' \
	  'make check-dsh  - offline DeepSeek Harness plugin typecheck/tests/package check' \
	  'make check-all  - core + UQA + DeepSeek Harness gates'
fmt:
	cargo fmt --all -- --check
test:
	cargo test --all-targets --no-default-features
test-uqa:
	cargo test --all-targets --features uqa
clippy:
	cargo clippy --all-targets --no-default-features -- -D warnings
clippy-uqa:
	cargo clippy --all-targets --features uqa -- -D warnings
static:
	python3 scripts/static_validate.py
check-dsh:
	cd integrations/deepseek-harness && npm run check:offline
check: fmt test clippy static
check-uqa: check test-uqa clippy-uqa
check-all: check-uqa check-dsh
