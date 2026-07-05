CARGO_BIN := $(HOME)/.cargo/bin

.PHONY: build install link uninstall run

build:
	cargo build --release

# Build + install the release binary, then ensure the short `s` symlink exists.
install:
	cargo install --path . --force
	@ln -sf "$(CARGO_BIN)/sucher" "$(CARGO_BIN)/s"
	@echo "installed: s -> sucher ($(CARGO_BIN))"

# Just (re)create the symlink without reinstalling.
link:
	@ln -sf "$(CARGO_BIN)/sucher" "$(CARGO_BIN)/s"
	@echo "linked: s -> sucher"

uninstall:
	@rm -f "$(CARGO_BIN)/s"
	cargo uninstall sucher || true

run:
	cargo run -- samples/sample.md
