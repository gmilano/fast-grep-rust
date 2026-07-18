.PHONY: build release test bench demo bench-agent clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

bench:
	cargo bench

demo: release
	bash scripts/demo/demo.sh

bench-agent: release
	bash scripts/bench-agent/run.sh

clean:
	cargo clean
