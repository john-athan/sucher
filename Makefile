CARGO_BIN := $(HOME)/.cargo/bin

.PHONY: build install link uninstall run

build:
	cargo build --release

# Build + install the release binary, then ensure the short `v` symlink exists.
install:
	cargo install --path . --force
	@ln -sf "$(CARGO_BIN)/vellum" "$(CARGO_BIN)/v"
	@echo "installed: v -> vellum ($(CARGO_BIN))"

# Just (re)create the symlink without reinstalling.
link:
	@ln -sf "$(CARGO_BIN)/vellum" "$(CARGO_BIN)/v"
	@echo "linked: v -> vellum"

uninstall:
	@rm -f "$(CARGO_BIN)/v"
	cargo uninstall vellum || true

run:
	cargo run -- sample.md
