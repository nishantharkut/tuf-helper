#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_DIR="$DATA_DIR/applications"
ICON_DIR="$DATA_DIR/icons/hicolor/scalable/apps"

BUILD=1
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --no-build) BUILD=0 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help)
            cat <<'EOF'
Usage: ./scripts/install.sh [OPTIONS]

Install TUF-Helper for the current desktop user (binary, SVG icon, and desktop launcher).

Options:
  --no-build    Install existing target/release/tuf-gui without running cargo build
  --uninstall   Remove installed binary, icon, and desktop entry
  -h, --help    Show this help message
EOF
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

refresh_caches() {
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$DATA_DIR/icons/hicolor" >/dev/null 2>&1 || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
    fi
}

if [[ "$UNINSTALL" -eq 1 ]]; then
    rm -f "$BIN_DIR/tuf-gui" "$ICON_DIR/tuf-helper.svg" "$APP_DIR/tuf-helper.desktop"
    refresh_caches
    echo "Removed TUF-Helper from $BIN_DIR and $DATA_DIR."
    exit 0
fi

if [[ "$BUILD" -eq 1 ]]; then
    echo "Building TUF-Helper release binary..."
    cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" -p tuf-gui --locked
fi

BIN_SRC="$ROOT_DIR/target/release/tuf-gui"
if [[ ! -x "$BIN_SRC" ]]; then
    echo "Release binary not found at $BIN_SRC. Run without --no-build first." >&2
    exit 1
fi

install -d "$BIN_DIR" "$ICON_DIR" "$APP_DIR"
install -m 0755 "$BIN_SRC" "$BIN_DIR/tuf-gui"
if command -v strip >/dev/null 2>&1; then
    strip -s "$BIN_DIR/tuf-gui" || true
fi

install -m 0644 "$ROOT_DIR/assets/tuf-helper.svg" "$ICON_DIR/tuf-helper.svg"
sed "s|^Exec=.*|Exec=$BIN_DIR/tuf-gui|" "$ROOT_DIR/packaging/tuf-helper.desktop" > "$APP_DIR/tuf-helper.desktop"
chmod 0644 "$APP_DIR/tuf-helper.desktop"
refresh_caches

echo "Installed TUF-Helper to $BIN_DIR/tuf-gui"
echo "Launcher installed at $APP_DIR/tuf-helper.desktop"
if [[ ! -r /dev/i2c-0 ]] && ! ls /dev/i2c-* >/dev/null 2>&1; then
    echo "Note: No /dev/i2c-* devices detected. Load i2c-dev for external monitor DDC/CI support."
fi
