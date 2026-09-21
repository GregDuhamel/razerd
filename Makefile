.PHONY: build check install uninstall install-watch uninstall-watch install-battery uninstall-battery clean

# System-wide by default: the binary must be root-owned, because
# razerd-battery.service (a system unit holding a /dev/uhid descriptor) runs
# it — a user-writable binary there would hand that descriptor to any process
# of yours. Targets that write under PREFIX or /etc need sudo; targets that
# manage *user* units must run as yourself.
PREFIX          ?= /usr/local
BINDIR          := $(PREFIX)/bin
BIN             := $(BINDIR)/razerd
UNIT_DIR        := $(HOME)/.config/systemd/user
WATCH_UNIT      := $(UNIT_DIR)/razerd-watch.service
SYSTEM_UNIT_DIR := /etc/systemd/system
BATTERY_UNIT    := $(SYSTEM_UNIT_DIR)/razerd-battery.service

# The units in contrib/ hard-code /usr/local/bin; follow PREFIX when it differs.
install_unit = sed 's|/usr/local/bin|$(BINDIR)|g' $(1) | install -Dm 0644 /dev/stdin $(2)

require_root = @if [ "$$(id -u)" != 0 ]; then echo "✗ '$@' manages a system unit — run it with sudo"; exit 1; fi
require_user = @if [ "$$(id -u)" = 0 ]; then echo "✗ '$@' manages your user units — run it without sudo"; exit 1; fi
require_bin  = @if [ ! -x $(BIN) ]; then echo "✗ $(BIN) not found — run 'make build && sudo make install' first"; exit 1; fi

build:
	cargo build --release

# What CI runs, locally.
check:
	cargo fmt --all -- --check
	cargo check --locked --all-targets
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked --all-targets
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items

# No dependency on `build`: this runs under sudo, and cargo must not.
install:
	@if [ ! -f target/release/razerd ]; then echo "✗ target/release/razerd not found — run 'make build' first (without sudo)"; exit 1; fi
	install -Dm 0755 target/release/razerd $(BIN)
	@echo "✓ installed: $(BIN)"

uninstall:
	rm -f $(BIN)
	@echo "✓ removed: $(BIN)"

install-watch:
	$(require_user)
	$(require_bin)
	$(call install_unit,contrib/razerd-watch.service,$(WATCH_UNIT))
	systemctl --user daemon-reload
	systemctl --user enable razerd-watch.service
	systemctl --user restart razerd-watch.service
	@echo "✓ watch service enabled (re-applies color the moment the mouse wakes)"
	@echo "  Run 'sudo loginctl enable-linger $$USER' to start at boot without logging in"
	@echo "  Edit color: systemctl --user edit razerd-watch.service  (change --watch <color>)"

uninstall-watch:
	$(require_user)
	-systemctl --user disable --now razerd-watch.service
	rm -f $(WATCH_UNIT)
	systemctl --user daemon-reload
	@echo "✓ watch service removed"

# System unit: run with sudo.
install-battery:
	$(require_root)
	$(require_bin)
	@if command -v selinuxenabled >/dev/null 2>&1 && selinuxenabled; then \
		echo "SELinux: installing the razerd-uhid policy module (takes a few seconds)"; \
		semodule -i contrib/razerd-uhid.cil; \
	fi
	$(call install_unit,contrib/razerd-battery.service,$(BATTERY_UNIT))
	systemctl daemon-reload
	systemctl enable razerd-battery.service
	systemctl restart razerd-battery.service
	@echo "✓ battery bridge enabled — the mouse shows up in UPower / the desktop's power applet"
	@echo "  Logs: journalctl -u razerd-battery.service"

uninstall-battery:
	$(require_root)
	-systemctl disable --now razerd-battery.service
	rm -f $(BATTERY_UNIT)
	systemctl daemon-reload
	-@if command -v semodule >/dev/null 2>&1; then semodule -r razerd-uhid 2>/dev/null; fi
	@echo "✓ battery bridge removed"

clean:
	cargo clean
