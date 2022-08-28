
build:
	cargo build

release:
	cargo release

test:
	cargo test

rebuild_certs:
	cd resources/test_certs && make

lint:
	cargo clippy -- -Dwarnings

format:
	cargo fmt

check_format:
	cargo fmt --check

verify: lint check_format test

