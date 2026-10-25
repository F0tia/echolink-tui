#!/usr/bin/env bash
# Build echolink-tui and echolink-gui and install them for the current user.
# Nothing personal is installed: on first start the app asks for your own
# callsign, password, name and location.
#
#   ./install.sh            desktop app + terminal version
#   ./install.sh --tui-only terminal version only (no GUI libraries)
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v cargo >/dev/null; then
    echo "Rust isn't installed. Get it from https://rustup.rs and run this again." >&2
    exit 1
fi

bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"

if [[ "${1:-}" == "--tui-only" ]]; then
    cargo build --release --no-default-features
    install -Dm755 target/release/echolink-tui "$bin/echolink-tui"
    echo "Installed echolink-tui to $bin. Run 'echolink-tui setup' to enter your station details."
    exit 0
fi

cargo build --release
install -Dm755 target/release/echolink-gui "$bin/echolink-gui"
install -Dm755 target/release/echolink-tui "$bin/echolink-tui"
install -Dm644 packaging/echolink-gui.desktop "$data/applications/echolink-gui.desktop"
install -Dm644 packaging/echolink-gui.svg "$data/icons/hicolor/scalable/apps/echolink-gui.svg"
command -v update-desktop-database >/dev/null && update-desktop-database -q "$data/applications" || true

echo "Installed to $bin. Open EchoLink from your app menu; on first start it asks for your callsign."
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin isn't on your PATH, so add it to run echolink-tui from a terminal." ;;
esac
