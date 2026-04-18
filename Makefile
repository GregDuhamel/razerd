.PHONY: build install uninstall clean

PREFIX ?= $(HOME)/.local
BIN    := $(PREFIX)/bin/razerd

build:
	cargo build --release

install: build
	install -Dm 0755 target/release/razerd $(BIN)
	@echo "✓ installed: $(BIN)"

uninstall:
	rm -f $(BIN)
	@echo "✓ removed: $(BIN)"

clean:
	cargo clean
