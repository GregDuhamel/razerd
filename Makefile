.PHONY: build install uninstall clean

PREFIX ?= /usr/local
BIN    := $(PREFIX)/bin/razerd

build:
	cargo build --release

install: build
	sudo install -m 0755 target/release/razerd $(BIN)
	@echo "✓ installed: $(BIN)"

uninstall:
	sudo rm -f $(BIN)
	@echo "✓ removed: $(BIN)"

clean:
	cargo clean
