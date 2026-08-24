CARGO_BIN := $(HOME)/.cargo/bin

# The fast PDF path (pdfium, ADR 0015) needs no Makefile plumbing: build.rs
# fetches the pinned, checksum-verified libpdfium for the target and embeds it in
# the binary, so a plain `cargo build` / `cargo install` is self-contained. To
# build offline, pre-place the library at `vendor/pdfium/<lib>` or point
# `SUCHER_PDFIUM_LIB` at it; set `SUCHER_PDFIUM_NO_EMBED=1` to skip embedding
# entirely (PDFs then use the poppler fallback).

.PHONY: build install link uninstall run notices

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

# Regenerate the dependency notices that ship with the release binary.
# The normalisation is load bearing: upstream license texts carry CRLF, trailing
# spaces and stray blank lines, and not identically on every machine that
# extracts them, so without it the CI staleness check compares whitespace
# forever. A notice has to carry the text, not its incidental spacing.
# The generator version is an input to the output, so it is pinned in
# `.cargo-about-version` and CI installs exactly that one. Generating with a
# different version produces a file CI will reject, which is a confusing way to
# find out; fail here instead, with the command that fixes it.
notices:
	@want=$$(cat .cargo-about-version); \
	have=$$(cargo about --version 2>/dev/null | awk '{print $$2}'); \
	if [ "$$have" != "$$want" ]; then \
		echo "notices: need cargo-about $$want, found $${have:-none}"; \
		echo "  cargo install cargo-about --locked --features cli --version $$want"; \
		exit 1; \
	fi
	cargo about generate about.hbs -o THIRD_PARTY_LICENSES.md
	@perl -0777 -pi -e 's/\r\n/\n/g; s/[ \t]+$$//mg; s/\n{3,}/\n\n/g' THIRD_PARTY_LICENSES.md
	@echo "notices: THIRD_PARTY_LICENSES.md regenerated"
