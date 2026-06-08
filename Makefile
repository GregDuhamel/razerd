.PHONY: build install uninstall install-watch uninstall-watch install-notify uninstall-notify clean

PREFIX          ?= $(HOME)/.local
BIN             := $(PREFIX)/bin/razerd
NOTIFY_BIN      := $(PREFIX)/bin/razerd-battery-notify
UNIT_DIR        := $(HOME)/.config/systemd/user
WATCH_UNIT      := $(UNIT_DIR)/razerd-watch.service
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

install-watch: install
	install -Dm 0644 razerd-watch.service $(WATCH_UNIT)
	systemctl --user daemon-reload
	systemctl --user enable --now razerd-watch.service
	@echo "✓ watch service enabled (re-applies color the moment the mouse wakes)"
	@echo "  Run 'sudo loginctl enable-linger $$USER' to start at boot without logging in"
	@echo "  Edit color: systemctl --user edit razerd-watch.service  (change --watch <color>)"

uninstall-watch:
	-systemctl --user disable --now razerd-watch.service
	rm -f $(WATCH_UNIT)
	systemctl --user daemon-reload
	@echo "✓ watch service removed"

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
