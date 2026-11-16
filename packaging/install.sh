#!/bin/sh
# Builds Shreader and installs it for the current user (no root needed):
#   ~/.local/bin/shreader
#   ~/.local/share/applications/io.github.bmharks.Shreader.desktop
#   ~/.local/share/icons/hicolor/scalable/apps/io.github.bmharks.Shreader.svg
#
# Usage: packaging/install.sh            build and install
#        packaging/install.sh --uninstall remove those three files
#                                         (your archive in ~/.local/share/shreader stays)
set -eu

APP_ID=io.github.bmharks.Shreader
BIN_DIR="$HOME/.local/bin"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS_DIR="$DATA_HOME/applications"
ICON_DIR="$DATA_HOME/icons/hicolor/scalable/apps"

cd "$(dirname "$0")/.."

refresh_menus() {
    # Let the desktop notice the change (both are optional)
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS_DIR" || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "$DATA_HOME/icons/hicolor" 2>/dev/null || true
    # KDE Plasma keeps icons it looked up in memory, including "not found";
    # without this, the menu and taskbar show no icon until you log out
    command -v dbus-send >/dev/null 2>&1 &&
        dbus-send --session --type=signal /KIconLoader org.kde.KIconLoader.iconChanged int32:0 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN_DIR/shreader" "$APPS_DIR/$APP_ID.desktop" "$ICON_DIR/$APP_ID.svg"
    refresh_menus
    echo "Shreader removed. Your archive in $DATA_HOME/shreader was left in place."
    exit 0
fi

cargo build --release --locked

install -Dm755 target/release/shreader "$BIN_DIR/shreader"
install -Dm644 "packaging/$APP_ID.desktop" "$APPS_DIR/$APP_ID.desktop"
install -Dm644 "packaging/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg"
refresh_menus

echo "Installed. Shreader is in your applications menu."
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR isn't on your PATH, so the menu entry may not start it. Add it to PATH, or edit Exec= in $APPS_DIR/$APP_ID.desktop to the full path." ;;
esac
