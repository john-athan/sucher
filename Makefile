CARGO_BIN := $(HOME)/.cargo/bin

# --- pdfium runtime library (ADR 0015) -------------------------------------
# sucher renders PDFs with pdfium (Chrome's engine) when libpdfium is present,
# falling back to poppler otherwise. The library is loaded at runtime, never
# linked, so we just fetch the prebuilt shared object and place it beside the
# binary. Pinned to a specific bblanchon/pdfium-binaries release for a
# reproducible, checksum-verified supply chain.
PDFIUM_TAG   := chromium/7961
PDFIUM_DIR   := vendor/pdfium
UNAME_S      := $(shell uname -s)
UNAME_M      := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
  PDFIUM_LIBFILE := libpdfium.dylib
  ifeq ($(UNAME_M),arm64)
    PDFIUM_ASSET  := pdfium-mac-arm64
    PDFIUM_SHA256 := 1193a771e0bd934530afa3df73a0d44551d8f4078442e290054e6dd38ded960f
  else
    PDFIUM_ASSET  := pdfium-mac-x64
    PDFIUM_SHA256 :=
  endif
else
  PDFIUM_LIBFILE := libpdfium.so
  ifeq ($(UNAME_M),aarch64)
    PDFIUM_ASSET  := pdfium-linux-arm64
    PDFIUM_SHA256 :=
  else
    PDFIUM_ASSET  := pdfium-linux-x64
    PDFIUM_SHA256 :=
  endif
endif

PDFIUM_URL := https://github.com/bblanchon/pdfium-binaries/releases/download/$(PDFIUM_TAG)/$(PDFIUM_ASSET).tgz
PDFIUM_LIB := $(PDFIUM_DIR)/$(PDFIUM_LIBFILE)

.PHONY: build install link uninstall run pdfium

# Fetch libpdfium into vendor/ (once). Verifies the SHA-256 when one is pinned
# for this platform; otherwise trusts the pinned release tag over TLS and warns.
pdfium: $(PDFIUM_LIB)
$(PDFIUM_LIB):
	@mkdir -p "$(PDFIUM_DIR)"
	@echo "fetching $(PDFIUM_ASSET) ($(PDFIUM_TAG))…"
	@curl -sL --fail "$(PDFIUM_URL)" -o "$(PDFIUM_DIR)/pdfium.tgz"
	@if [ -n "$(PDFIUM_SHA256)" ]; then \
		echo "$(PDFIUM_SHA256)  $(PDFIUM_DIR)/pdfium.tgz" | shasum -a 256 -c - \
		  || { echo "pdfium checksum mismatch — aborting"; rm -f "$(PDFIUM_DIR)/pdfium.tgz"; exit 1; }; \
	else \
		echo "warning: no pinned checksum for $(PDFIUM_ASSET); trusting the release tag over TLS"; \
	fi
	@tar xzf "$(PDFIUM_DIR)/pdfium.tgz" -C "$(PDFIUM_DIR)" lib/$(PDFIUM_LIBFILE)
	@mv "$(PDFIUM_DIR)/lib/$(PDFIUM_LIBFILE)" "$(PDFIUM_LIB)"
	@rm -rf "$(PDFIUM_DIR)/pdfium.tgz" "$(PDFIUM_DIR)/lib"
	@echo "pdfium ready: $(PDFIUM_LIB)"

# Build the release binary and drop libpdfium beside both target binaries so
# `cargo run` / `cargo run --release` resolve it (sucher looks next to its exe).
build: pdfium
	cargo build --release
	@mkdir -p target/release target/debug
	@cp -f "$(PDFIUM_LIB)" target/release/ 2>/dev/null || true
	@cp -f "$(PDFIUM_LIB)" target/debug/ 2>/dev/null || true

# Build + install the release binary AND libpdfium beside it, then ensure the
# short `s` symlink exists. Installing the library is what makes the fast PDF
# path work after a plain `make install`.
install: pdfium
	cargo install --path . --force
	@cp -f "$(PDFIUM_LIB)" "$(CARGO_BIN)/$(PDFIUM_LIBFILE)"
	@ln -sf "$(CARGO_BIN)/sucher" "$(CARGO_BIN)/s"
	@echo "installed: s -> sucher ($(CARGO_BIN)), with $(PDFIUM_LIBFILE)"

# Just (re)create the symlink without reinstalling.
link:
	@ln -sf "$(CARGO_BIN)/sucher" "$(CARGO_BIN)/s"
	@echo "linked: s -> sucher"

uninstall:
	@rm -f "$(CARGO_BIN)/s" "$(CARGO_BIN)/$(PDFIUM_LIBFILE)"
	cargo uninstall sucher || true

run: pdfium
	@mkdir -p target/debug && cp -f "$(PDFIUM_LIB)" target/debug/ 2>/dev/null || true
	cargo run -- samples/sample.md
