.PHONY: build check clean

build:
	cargo build --release --target wasm32-wasip2

check:
	cargo check --target wasm32-wasip2

clean:
	cargo clean
