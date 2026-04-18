.PHONY: build install uninstall install-service uninstall-service install-notify uninstall-notify clean

PREFIX          ?= $(HOME)/.local
BIN             := $(PREFIX)/bin/razerd
NOTIFY_BIN      := $(PREFIX)/bin/razerd-battery-notify
UNIT_DIR        := $(HOME)/.config/systemd/user
UNIT            := $(UNIT_DIR)/razerd.service
TIMER           := $(UNIT_DIR)/razerd.timer
NOTIFY_UNIT     := $(UNIT_DIR)/razerd-battery-notify.service
NOTIFY_TIMER    := $(UNIT_DIR)/razerd-battery-notify.timer

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

install-notify: install
	install -Dm 0755 contrib/razerd-battery-notify $(NOTIFY_BIN)
	install -Dm 0644 contrib/razerd-battery-notify.service $(NOTIFY_UNIT)
	install -Dm 0644 contrib/razerd-battery-notify.timer $(NOTIFY_TIMER)
	systemctl --user daemon-reload
	systemctl --user enable --now razerd-battery-notify.timer
	@echo "✓ low-battery notifier enabled (checks every 5 min, threshold 20%)"
	@echo "  Override threshold: systemctl --user edit razerd-battery-notify.service"
	@echo "  and add [Service] Environment=RAZERD_LOW_BATTERY=15"

uninstall-notify:
	-systemctl --user disable --now razerd-battery-notify.timer razerd-battery-notify.service
	rm -f $(NOTIFY_BIN) $(NOTIFY_UNIT) $(NOTIFY_TIMER)
	systemctl --user daemon-reload
	@echo "✓ low-battery notifier removed"

clean:
	cargo clean
