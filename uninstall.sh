#!/usr/bin/env bash
# Remove Ignite.
#
#   ./uninstall.sh                 remove Ignite from /usr/local
#   PREFIX=/usr ./uninstall.sh     if you installed with a different PREFIX
#   ./uninstall.sh --legacy        also remove the old names (bootforge, gdd)
#   ./uninstall.sh --clean-build   also delete the build folder (target/) here
#   DRY_RUN=1 ./uninstall.sh       only show what would be removed
#
# Packages installed as dependencies (parted, polkit, ...) are left alone:
# other programs use them. Ignite keeps no settings or data of its own.
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
DRY_RUN="${DRY_RUN:-0}"
LEGACY=0
CLEAN_BUILD=0

usage() { sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; }

for arg in "$@"; do
    case "$arg" in
        --legacy) LEGACY=1 ;;
        --clean-build) CLEAN_BUILD=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

HERE="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    if ! command -v sudo >/dev/null 2>&1 && [ "$DRY_RUN" != "1" ]; then
        echo "ERROR: 'sudo' was not found. Run this script as root, or install sudo." >&2
        exit 1
    fi
    SUDO="sudo"
fi

removed=0
rm_file() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        if [ "$DRY_RUN" = "1" ]; then
            echo "  would remove $1"
        else
            $SUDO rm -f "$1"
            echo "  removed $1"
        fi
        removed=$((removed + 1))
    fi
}

remove_app() {   # $1 = program/icon name
    local name="$1" size
    rm_file "$PREFIX/bin/$name"
    rm_file "$PREFIX/share/applications/$name.desktop"
    for size in 512 256 128 64 48 32; do
        rm_file "$PREFIX/share/icons/hicolor/${size}x${size}/apps/$name.png"
    done
    rm_file "$PREFIX/share/icons/hicolor/scalable/apps/$name.svg"
}

echo "==> Removing Ignite from $PREFIX"
remove_app ignite
if [ "$LEGACY" = "1" ]; then
    echo "==> Removing older names (bootforge, gdd)"
    remove_app bootforge
    remove_app gdd
fi

if [ "$removed" -eq 0 ]; then
    echo "Nothing to remove under $PREFIX."
    other="$(command -v ignite 2>/dev/null || true)"
    if [ -n "$other" ]; then
        echo "But an 'ignite' exists at $other: if you installed with another PREFIX,"
        echo "run for example:  PREFIX=$(dirname "$(dirname "$other")") ./uninstall.sh"
    fi
elif [ "$DRY_RUN" != "1" ]; then
    # Refresh desktop caches so the menu entry and icon disappear right away.
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -d "$PREFIX/share/icons/hicolor" ]; then
        $SUDO gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$PREFIX/share/applications" ]; then
        $SUDO update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    fi
    echo "==> Done: $removed file(s) removed."
else
    echo "==> Dry run: $removed file(s) would be removed."
fi

if [ "$CLEAN_BUILD" = "1" ]; then
    if [ -d "$HERE/target" ] && [ -f "$HERE/Cargo.toml" ]; then
        if [ "$DRY_RUN" = "1" ]; then
            echo "  would remove $HERE/target"
        else
            rm -rf "$HERE/target"
            echo "  removed build folder $HERE/target"
        fi
    else
        echo "No build folder to remove."
    fi
fi
