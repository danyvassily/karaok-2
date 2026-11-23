.PHONY: build test bench devices run clean

build:
	./scripts/build-app.sh

test:
	cd rust && cargo test --workspace

bench:
	cd rust && cargo run --release -p karaok-cli -- bench-dsp

devices:
	cd rust && cargo run --release -p karaok-cli -- devices

run: build
	open build/KaraokParty.app

clean:
	rm -rf build/KaraokParty.app mac-app/.build rust/target
