#!/usr/bin/env bash
# ASTRA installer — user-local, no root required.
set -euo pipefail

PREFIX="${HOME}/.local"
BIN_SRC="$(dirname "$0")/astra"
BIN_DST="${PREFIX}/bin/astra"
DESKTOP_DST="${PREFIX}/share/applications/astra.desktop"
ICON_DST="${PREFIX}/share/icons/hicolor/scalable/apps/astra.svg"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN_DST" "$DESKTOP_DST" "$ICON_DST"
  echo "ASTRA uninstalled (config kept at ~/.config/astra)."
  exit 0
fi

if [[ ! -x "$BIN_SRC" ]]; then
  echo "error: $BIN_SRC not found — build it first: cargo build --release"
  exit 1
fi

install -Dm755 "$BIN_SRC" "$BIN_DST"
install -Dm644 "$(dirname "$0")/assets/astra.svg" "$ICON_DST"
# desktop file with absolute paths so any launcher environment can exec it
sed -e "s|^Exec=astra$|Exec=${BIN_DST}|" \
    -e "s|^Icon=astra$|Icon=${ICON_DST}|" \
    "$(dirname "$0")/assets/astra.desktop" > "$DESKTOP_DST"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "${PREFIX}/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -qtf "${PREFIX}/share/icons/hicolor" || true

echo "Installed:"
echo "  $BIN_DST"
echo "  $DESKTOP_DST"
echo "  $ICON_DST"
echo "Make sure ${PREFIX}/bin is in your PATH."
