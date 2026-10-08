.PHONY: build check install uninstall install-udev uninstall-udev install-daemon uninstall-daemon install-watch uninstall-watch install-battery uninstall-battery clean

# System-wide by default: the binary must be root-owned, because razerd.service
# (a system unit holding a /dev/uhid descriptor) runs it — a user-writable
# binary there would hand that descriptor to any process of yours. Targets that
# write under PREFIX or /etc need sudo; targets that manage *user* units must
# run as yourself.
PREFIX          ?= /usr/local
BINDIR          := $(PREFIX)/bin
BIN             := $(BINDIR)/razerd
UNIT_DIR        := $(HOME)/.config/systemd/user
WATCH_UNIT      := $(UNIT_DIR)/razerd-watch.service
SYSTEM_UNIT_DIR := /etc/systemd/system
DAEMON_UNIT     := $(SYSTEM_UNIT_DIR)/razerd.service
# ≤ 0.12: the battery bridge alone, which install-daemon replaces.
LEGACY_UNIT     := $(SYSTEM_UNIT_DIR)/razerd-battery.service
CONF            := /etc/razerd/razerd.conf
UDEV_RULES_DIR  := /etc/udev/rules.d
UDEV_RULES      := $(UDEV_RULES_DIR)/70-razerd.rules
# The dock's control interface, as the udev rule names it: the device unit
# razerd.service is bound to. systemctl takes the path for the unit name.
DOCK_NODE       := /dev/razer-dock

# The units in contrib/ hard-code /usr/local/bin; follow PREFIX when it differs.
install_unit = sed 's|/usr/local/bin|$(BINDIR)|g' $(1) | install -Dm 0644 /dev/stdin $(2)

require_root = @if [ "$$(id -u)" != 0 ]; then echo "✗ '$@' manages a system unit — run it with sudo"; exit 1; fi
require_user = @if [ "$$(id -u)" = 0 ]; then echo "✗ '$@' manages your user units — run it without sudo"; exit 1; fi
require_bin  = @if [ ! -x $(BIN) ]; then echo "✗ $(BIN) not found — run 'make build && sudo make install' first"; exit 1; fi

# The udev rule, and the group it names. The group is a system group (-r):
# udevd ≥ 258 deprecates device nodes owned by user groups. The trigger
# re-applies the rule to the dock if it is plugged in: ownership, the
# /dev/razer-dock symlink, and — the first time — the device unit that starts
# razerd.service.
define install_udev_rule
	groupadd -rf razerd
	install -Dm 0644 contrib/70-razerd.rules $(UDEV_RULES)
	udevadm control --reload
	udevadm trigger --subsystem-match=usb --subsystem-match=hidraw
	udevadm settle
endef

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

# udev rule granting the razerd group access to the dock: run with sudo. Adding
# yourself to the group is left to you — it needs your login name, not root's,
# and a fresh login to take effect.
install-udev:
	$(require_root)
	$(install_udev_rule)
	@echo "✓ udev rule installed: $(UDEV_RULES)"
	@echo "  Now: sudo usermod -aG razerd \$$USER   (then log out and back in)"
	@echo "  Verify with: razerd --check"

uninstall-udev:
	$(require_root)
	rm -f $(UDEV_RULES)
	udevadm control --reload
	udevadm trigger --subsystem-match=usb --subsystem-match=hidraw
	@echo "✓ udev rule removed (the razerd group is kept; 'groupdel razerd' drops it)"

# The daemon: one system unit, started by udev when the dock is plugged in.
# Run with sudo. Order matters: the unit must be known to systemd before the
# udev trigger creates the device unit that wants it.
install-daemon:
	$(require_root)
	$(require_bin)
	@if command -v selinuxenabled >/dev/null 2>&1 && selinuxenabled; then \
		echo "SELinux: installing the razerd-uhid policy module (takes a few seconds)"; \
		semodule -i contrib/razerd-uhid.cil; \
	fi
	@if [ -f $(LEGACY_UNIT) ]; then \
		echo "migrating: razerd-battery.service is razerd.service now"; \
		systemctl disable --now razerd-battery.service; \
		rm -f $(LEGACY_UNIT); \
	fi
	@if [ -f $(CONF) ]; then \
		echo "  kept: $(CONF) (the current example is contrib/razerd.conf)"; \
	else \
		install -Dm 0644 contrib/razerd.conf $(CONF); \
		echo "✓ installed: $(CONF)"; \
	fi
	$(call install_unit,contrib/razerd.service,$(DAEMON_UNIT))
	systemctl daemon-reload
	$(install_udev_rule)
	@if systemctl is-active --quiet razerd.service; then \
		systemctl restart razerd.service; \
		echo "✓ razerd.service restarted"; \
	elif systemctl is-active --quiet $(DOCK_NODE); then \
		systemctl start razerd.service; \
		echo "✓ razerd.service started"; \
	else \
		echo "  dock not plugged in: razerd.service starts when it is"; \
	fi
	@echo "✓ daemon installed — the mouse shows up in UPower / the desktop's power applet"
	@echo "  Held color: $(CONF)   Logs: journalctl -u razerd.service"
	@echo "  A razerd-watch.service user unit must not run alongside: make uninstall-watch"

uninstall-daemon:
	$(require_root)
	-systemctl stop razerd.service
	rm -f $(DAEMON_UNIT)
	systemctl daemon-reload
	-@if command -v semodule >/dev/null 2>&1; then semodule -r razerd-uhid 2>/dev/null; fi
	@echo "✓ daemon removed ($(CONF) and the udev rule are kept: 'make uninstall-udev' drops the rule)"

# The standalone alternative: a user unit holding the color, without the
# battery bridge. Not alongside razerd.service with --hold.
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
	@echo "  Not alongside razerd.service with --hold: one process must own the dock"

uninstall-watch:
	$(require_user)
	-systemctl --user disable --now razerd-watch.service
	rm -f $(WATCH_UNIT)
	systemctl --user daemon-reload
	@echo "✓ watch service removed"

# ≤ 0.12 names. install-battery is install-daemon; uninstall-battery removes
# the old razerd-battery.service alone (install-daemon does that as well).
install-battery: install-daemon

uninstall-battery:
	$(require_root)
	-systemctl disable --now razerd-battery.service
	rm -f $(LEGACY_UNIT)
	systemctl daemon-reload
	@echo "✓ razerd-battery.service removed (its replacement: sudo make install-daemon)"

clean:
	cargo clean
