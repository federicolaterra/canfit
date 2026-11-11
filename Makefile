CARGO ?= cargo

.PHONY: check fmt fmt-check clippy test build run

## check: format check, clippy with warnings denied, and tests (the CI gate)
check: fmt-check clippy test

## fmt: format all sources in place
fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings

test:
	$(CARGO) test --all-targets --all-features --locked

## build: optimized release binary in target/release/canfit
build:
	$(CARGO) build --release --locked

## run: run a debug build; pass arguments with ARGS="model.gguf --json"
run:
	$(CARGO) run -- $(ARGS)
