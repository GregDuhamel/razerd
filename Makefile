.PHONY: build install uninstall install-service uninstall-service clean

PREFIX      ?= $(HOME)/.local
BIN         := $(PREFIX)/bin/razerd
UNIT_DIR    := $(HOME)/.config/systemd/user
UNIT        := $(UNIT_DIR)/razerd.service

build:
	cargo build --release

install: build
	install -Dm 0755 target/release/razerd $(BIN)
	@echo "✓ installed: $(BIN)"

uninstall:
	rm -f $(BIN)
	@echo "✓ removed: $(BIN)"

install-service: install
	install -Dm 0644 razerd.service $(UNIT)
	systemctl --user daemon-reload
	systemctl --user enable --now razerd.service
	@echo "✓ service enabled and started"
	@echo "  Run 'sudo loginctl enable-linger $$USER' to apply the color at boot without logging in"
	@echo "  Edit color via: systemctl --user edit razerd.service"

uninstall-service:
	-systemctl --user disable --now razerd.service
	rm -f $(UNIT)
	systemctl --user daemon-reload
	@echo "✓ service removed"

clean:
	cargo clean
