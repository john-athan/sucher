CARGO_BIN := $(HOME)/.cargo/bin

# The fast PDF path (pdfium, ADR 0015) is resolved at runtime from beside the
# binary, so a plain `cargo build` carries no copy of it and starts ~110 ms
# faster. Put `libpdfium.dylib` next to the binary or in `/opt/homebrew/lib` to
# get it; without one, PDFs use the poppler fallback.
#
# `cargo install sucher --features embed-pdfium` bakes the pinned,
# checksum-verified library into the binary instead, for installs that can place
# no sidecar. That build pays the ~110 ms on every start, PDF or not. To build it
# offline, pre-place the library at `vendor/pdfium/<lib>` or point
# `SUCHER_PDFIUM_LIB` at it; `SUCHER_PDFIUM_NO_EMBED=1` skips embedding even with
# the feature on.

.PHONY: build install link uninstall run notices deny check

# Everything CI checks, in the order CI checks it, runnable here first. `oss
# check sucher` calls exactly this, and so does .github/workflows/ci.yml, so
# there is one definition of what "green" means rather than three.
check:
	cargo fmt --check
	cargo clippy -- -D warnings
	cargo test
	cargo build --release
	cargo build --no-default-features
	cargo test --no-default-features
	$(MAKE) deny
	$(MAKE) notices
	@git diff --quiet -- THIRD_PARTY_LICENSES.md || { \
		echo "THIRD_PARTY_LICENSES.md is out of date; `make notices` rewrote it."; \
		git diff --stat -- THIRD_PARTY_LICENSES.md; \
		exit 1; \
	}
	@echo "check: everything passes"


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
# The supply-chain gate, pinned for the same reason the notice generator is: the
# verdict is a property of the tool as much as of the tree. cargo-deny gains
# checks and revises advisory handling between versions, so an unpinned install
# means CI and this machine can disagree about a clean tree, and neither is
# wrong. `.cargo-deny-version` is the one place that says which.
deny:
	@want=$$(cat .cargo-deny-version); \
	have=$$(cargo deny --version 2>/dev/null | awk '{print $$2}'); \
	if [ "$$have" != "$$want" ]; then \
		echo "deny: need cargo-deny $$want, found $${have:-none}"; \
		echo "  cargo install cargo-deny --locked --version $$want"; \
		exit 1; \
	fi
	cargo deny check

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
