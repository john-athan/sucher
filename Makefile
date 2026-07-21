CARGO_BIN := $(HOME)/.cargo/bin

# The fast PDF path (pdfium, ADR 0015) needs no Makefile plumbing: build.rs
# fetches the pinned, checksum-verified libpdfium for the target and embeds it in
# the binary, so a plain `cargo build` / `cargo install` is self-contained. To
# build offline, pre-place the library at `vendor/pdfium/<lib>` or point
# `SUCHER_PDFIUM_LIB` at it; set `SUCHER_PDFIUM_NO_EMBED=1` to skip embedding
# entirely (PDFs then use the poppler fallback).

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
