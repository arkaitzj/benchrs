
build:
	cargo build

release:
	cargo release

test:
	cd resources/test_certs && make
	cargo test
