NAME := nearby-hpbars
RAW := target/wasm32-wasip2/release/cuo_nearby_hpbars.wasm
OUT ?= dist/$(NAME)

.PHONY: build wasm check clean

wasm:
	cargo build --release --target wasm32-wasip2

# Packages the mod folder (mod.wasm + mod.json) into dist/; copy it into the client's Data/Mods folder.
build: wasm
	mkdir -p $(OUT)
	cp $(RAW) $(OUT)/mod.wasm
	printf '{\n  "name": "$(NAME)",\n  "version": "0.1.1",\n  "wasm": "mod.wasm",\n  "ruleset": {}\n}\n' > $(OUT)/mod.json
	@echo ">> $(OUT)/mod.wasm"

check:
	cargo check --target wasm32-wasip2

clean:
	cargo clean
	rm -rf dist
