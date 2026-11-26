#!/usr/bin/env bash
# Install Quick Share for Dux Shell.
#
#   ./install.sh                     build the daemon, install it, link the plugin
#   ./install.sh --wifi-helper       also install the optional Wi-Fi helper (sudo)
#   ./install.sh --wifi-helper-only  only the Wi-Fi helper (sudo, or run as root)
#   ./install.sh --uninstall         remove the daemon and the plugin link
#   ./install.sh --remove-wifi-helper
#
# Everything is built from this repository; nothing is downloaded except the
# Rust crates cargo fetches. Only the Wi-Fi helper options need root. See
# README.md for what the helper does and why.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BIN_DIR="${HOME}/.local/bin"
BIN="${BIN_DIR}/dms-quickshare"
PLUGINS_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/dms/plugins"
HELPER_DIR=/usr/lib/dms-quickshare
POLKIT_POLICY=/usr/share/polkit-1/actions/io.github.jhannesreimann.dms-quickshare.policy
UDEV_RULE=/usr/lib/udev/rules.d/85-dms-quickshare.rules

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# rustup installs cargo to ~/.cargo/bin, which a desktop session's PATH may lack.
if ! have cargo && [ -x "${HOME}/.cargo/bin/cargo" ]; then
    PATH="${HOME}/.cargo/bin:${PATH}"
fi

# The helper options run under sudo, or directly when started as root (the
# plugin does that through pkexec).
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

usage() {
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

check_runtime() {
    have bluetoothctl || warn "BlueZ (bluetoothctl) not found: Bluetooth discovery will not work. Install 'bluez'."
    if have systemctl && ! systemctl is-active --quiet bluetooth 2>/dev/null; then
        warn "bluetooth.service is not running: sudo systemctl enable --now bluetooth"
    fi
    have nmcli || warn "NetworkManager not found: transfers between different networks stay on Bluetooth (slow)."
    have iw || warn "'iw' not found: the daemon cannot tell what your Wi-Fi card supports."
}

install_daemon() {
    have cargo || die "cargo not found. Install Rust (>= 1.89), e.g. 'sudo pacman -S rust' or https://rustup.rs"
    say "Building the daemon (release)..."
    (cd "${REPO}/daemon" && cargo build --release --locked)
    install -Dm755 "${REPO}/daemon/target/release/dms-quickshare" "${BIN}"
    say "Installed ${BIN}"

    # The old rquickshare-based daemon from earlier versions of this plugin.
    if [ -e "${BIN_DIR}/dms-quickshare-daemon" ]; then
        pkill -x dms-quickshare-daemon 2>/dev/null || true
        rm -f "${BIN_DIR}/dms-quickshare-daemon"
        say "Removed the old dms-quickshare-daemon"
    fi
    case ":${PATH}:" in
        *":${BIN_DIR}:"*) ;;
        *) warn "${BIN_DIR} is not in your PATH (the plugin finds the daemon anyway; the CLI needs it)." ;;
    esac
}

link_plugin() {
    mkdir -p "${PLUGINS_DIR}"
    # Already installed (cloned into the plugins folder or linked earlier)?
    for entry in "${PLUGINS_DIR}"/*; do
        [ -e "${entry}" ] || continue
        if [ "$(readlink -f "${entry}")" = "${REPO}" ]; then
            say "Plugin already installed at ${entry}"
            return
        fi
    done
    ln -s "${REPO}" "${PLUGINS_DIR}/quickshare"
    say "Linked the plugin to ${PLUGINS_DIR}/quickshare"
}

install_helper() {
    [ "$(id -u)" -eq 0 ] || have sudo || die "sudo is needed to install the Wi-Fi helper"
    for tool in iw ip wpa_supplicant dnsmasq pkexec; do
        have "${tool}" || [ -x "/usr/sbin/${tool}" ] || warn "'${tool}' not found: the Wi-Fi helper needs it"
    done
    say "Installing the Wi-Fi helper..."
    as_root install -Dm755 "${REPO}/system/wifi-helper" "${HELPER_DIR}/wifi-helper"
    as_root install -Dm644 "${REPO}/system/io.github.jhannesreimann.dms-quickshare.policy" "${POLKIT_POLICY}"
    as_root install -Dm644 "${REPO}/system/85-dms-quickshare.rules" "${UDEV_RULE}"
    if have udevadm; then
        as_root udevadm control --reload || true
    fi
    say "Installed ${HELPER_DIR}/wifi-helper, its polkit policy and udev rule"
}

remove_helper() {
    [ "$(id -u)" -eq 0 ] || have sudo || die "sudo is needed to remove the Wi-Fi helper"
    as_root "${HELPER_DIR}/wifi-helper" stop-ap 2>/dev/null || true
    as_root rm -f "${HELPER_DIR}/wifi-helper" "${POLKIT_POLICY}" "${UDEV_RULE}"
    as_root rmdir "${HELPER_DIR}" 2>/dev/null || true
    if have udevadm; then
        as_root udevadm control --reload || true
    fi
    say "Removed the Wi-Fi helper"
}

uninstall() {
    pkill -x dms-quickshare 2>/dev/null || true
    rm -f "${BIN}" "${BIN_DIR}/dms-quickshare-daemon"
    for entry in "${PLUGINS_DIR}"/*; do
        [ -L "${entry}" ] && [ "$(readlink -f "${entry}")" = "${REPO}" ] && rm -f "${entry}" && say "Removed ${entry}"
    done
    say "Uninstalled. Settings remain in ~/.config/dms-quickshare (delete it to reset)."
    [ -e "${HELPER_DIR}/wifi-helper" ] && say "The Wi-Fi helper is still installed: ./install.sh --remove-wifi-helper"
    return 0
}

case "${1:-}" in
    "")
        check_runtime
        install_daemon
        link_plugin
        ;;
    --wifi-helper)
        check_runtime
        install_daemon
        link_plugin
        install_helper
        ;;
    --wifi-helper-only)
        install_helper
        exit 0
        ;;
    --uninstall)
        uninstall
        exit 0
        ;;
    --remove-wifi-helper)
        remove_helper
        exit 0
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage
        exit 2
        ;;
esac

say ""
say "Done. Next steps:"
say "  1. Restart the shell so it picks up the plugin: dms restart"
say "  2. Settings > Plugins: enable Quick Share and add its widget to the bar"
say "  3. Check your setup: dms-quickshare doctor"
