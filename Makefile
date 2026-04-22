.PHONY: build release run cli test clippy install clean check

build:
	cargo build --workspace

release:
	cargo build --release --workspace

run:
	@test -n "$(PDF)" || (echo "usage: make run PDF=/path/to/file.pdf"; exit 1)
	cargo run --release --bin svreader -- "$(PDF)"

cli:
	@test -n "$(ARGS)" || (echo "usage: make cli ARGS='info foo.pdf'"; exit 1)
	cargo run --release --bin svreader-cli -- $(ARGS)

test:
	cargo test --workspace

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

check:
	cargo check --workspace

install:
	cargo install --path .

clean:
	cargo clean
