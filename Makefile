.PHONY: build install uninstall install-service uninstall-service clean

PREFIX      ?= $(HOME)/.local
BIN         := $(PREFIX)/bin/razerd
UNIT_DIR    := $(HOME)/.config/systemd/user
UNIT        := $(UNIT_DIR)/razerd.service
TIMER       := $(UNIT_DIR)/razerd.timer

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
	install -Dm 0644 razerd.timer $(TIMER)
	systemctl --user daemon-reload
	systemctl --user enable --now razerd.service razerd.timer
	@echo "✓ service + 30s timer enabled (re-applies color on wireless reconnects)"
	@echo "  Run 'sudo loginctl enable-linger $$USER' to start at boot without logging in"
	@echo "  Edit color:    systemctl --user edit razerd.service"
	@echo "  Edit interval: systemctl --user edit razerd.timer"

uninstall-service:
	-systemctl --user disable --now razerd.timer razerd.service
	rm -f $(UNIT) $(TIMER)
	systemctl --user daemon-reload
	@echo "✓ service and timer removed"

clean:
	cargo clean
