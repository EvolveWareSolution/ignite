#!/usr/bin/env bash
# Ignite installer: Fedora (dnf), Arch (pacman), Debian/Ubuntu (apt), openSUSE (zypper).
#
#   ./install.sh              install dependencies + build + install to /usr/local
#   ./install.sh --no-deps    skip the package-manager step
#   PREFIX=/usr ./install.sh  install somewhere else
#   DRY_RUN=1 ./install.sh    only print what would be done
#   DISTRO=arch ./install.sh  force fedora | arch | debian | suse (default: auto-detect)
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
DRY_RUN="${DRY_RUN:-0}"
INSTALL_DEPS=1

OS_RELEASE="${OS_RELEASE:-/etc/os-release}"
SYS_BIN="${SYS_BIN:-/usr/bin}"
RUST_DIRS_GLOB="${RUST_DIRS_GLOB:-/usr/lib/rust-1.*/bin}"
MIN_CARGO_MINOR=85   # dependencies need Cargo 1.85+ (Rust 2024 edition)

# Commands the program calls at run time.
RUNTIME_CMDS=(pkexec lsblk umount wipefs parted mkfs.vfat udisksctl udevadm)

usage() {
    sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
}

for arg in "$@"; do
    case "$arg" in
        --no-deps) INSTALL_DEPS=0 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

run() {
    if [ "$DRY_RUN" = "1" ]; then
        echo "+ $*"
    else
        "$@"
    fi
}

cd "$(dirname "$(readlink -f "$0")")"

# ------------------------------------------------------------ privileges
# Use sudo when not root; if started with sudo, build as the original user so
# target/ does not become root-owned.
SUDO=""
BUILD_AS=()
if [ "$(id -u)" -ne 0 ]; then
    if ! command -v sudo >/dev/null 2>&1 && [ "$DRY_RUN" != "1" ]; then
        echo "ERROR: 'sudo' was not found. Run this script as root, or install sudo." >&2
        exit 1
    fi
    SUDO="sudo"
elif [ -n "${SUDO_USER:-}" ]; then
    BUILD_AS=(sudo -u "$SUDO_USER" -H)
fi

# ----------------------------------------------------- distro detection
detect_distro() {
    if [ -n "${DISTRO:-}" ]; then echo "$DISTRO"; return; fi
    local id="" like=""
    if [ -r "$OS_RELEASE" ]; then
        id="$( . "$OS_RELEASE"; echo "${ID:-}" )"
        like="$( . "$OS_RELEASE"; echo "${ID_LIKE:-}" )"
    fi
    case " $id $like " in
        *" fedora "*|*" rhel "*|*" centos "*) echo fedora; return ;;
        *" arch "*)                          echo arch;   return ;;
        *" debian "*|*" ubuntu "*)           echo debian; return ;;
        *" suse "*|*" opensuse "*|*" sles "*|*" opensuse-leap "*|*" opensuse-tumbleweed "*)
                                             echo suse;   return ;;
    esac
    if   command -v dnf     >/dev/null 2>&1; then echo fedora
    elif command -v pacman  >/dev/null 2>&1; then echo arch
    elif command -v apt-get >/dev/null 2>&1; then echo debian
    elif command -v zypper  >/dev/null 2>&1; then echo suse
    else echo unknown
    fi
}
DISTRO="$(detect_distro)"

# Package names per family. On Debian an entry may list alternatives with "|"
# (names differ between releases); the first one the repositories know is used.
case "$DISTRO" in
    fedora)
        PKGS=(rust cargo gcc pkgconf-pkg-config fontconfig-devel libxkbcommon-devel wayland-devel
              vulkan-loader mesa-vulkan-drivers xdg-desktop-portal polkit udisks2 parted dosfstools util-linux)
        RUST_HINT="sudo dnf install rust cargo"
        ;;
    arch)
        # base-devel brings gcc, make and pkgconf. Headers ship in the main packages.
        PKGS=(rust base-devel fontconfig libxkbcommon wayland
              vulkan-icd-loader mesa xdg-desktop-portal polkit udisks2 parted dosfstools util-linux)
        RUST_HINT="sudo pacman -S rust   (or, with rustup: rustup default stable)"
        ;;
    debian)
        PKGS=(rustc cargo build-essential pkg-config "libfontconfig-dev|libfontconfig1-dev"
              libxkbcommon-dev libwayland-dev libvulkan1 mesa-vulkan-drivers xdg-desktop-portal
              "pkexec|policykit-1" "polkitd|policykit-1" udisks2 parted dosfstools util-linux)
        RUST_HINT="install a newer Rust with rustup (https://rustup.rs): Debian/Ubuntu releases often ship an older one"
        ;;
    suse)
        # "pkg-config" is a capability: pkgconf-pkg-config on Tumbleweed, pkg-config on Leap.
        PKGS=(rust cargo gcc pkg-config fontconfig-devel libxkbcommon-devel wayland-devel
              libvulkan1 Mesa-dri xdg-desktop-portal polkit udisks2 parted dosfstools util-linux)
        RUST_HINT="sudo zypper install rust cargo   (older Leap releases ship an older Rust: sudo zypper install rustup && rustup default stable)"
        ;;
    *)
        PKGS=()
        RUST_HINT="install Rust 1.85+ with rustup (https://rustup.rs)"
        ;;
esac

# Extra tools for the Windows page (wimlib, NTFS/exFAT formatting, GRUB for
# legacy BIOS boot). Installed one by one, best effort: a package that does
# not exist on your release must not stop the main install.
case "$DISTRO" in
    fedora) WIN_PKGS=(wimlib-utils ntfsprogs ntfs-3g exfatprogs grub2-pc curl) ;;
    arch)   WIN_PKGS=(wimlib ntfs-3g exfatprogs grub curl) ;;
    debian) WIN_PKGS=(wimtools ntfs-3g exfatprogs grub-pc-bin curl) ;;
    suse)   WIN_PKGS=(wimlib ntfs-3g ntfs-3g_ntfsprogs exfatprogs grub2-i386-pc curl) ;;
    *)      WIN_PKGS=() ;;
esac

install_win_pkgs() {
    [ "${#WIN_PKGS[@]}" -gt 0 ] || return 0
    echo "==> Installing the optional tools for the Windows page (best effort)"
    local p
    for p in "${WIN_PKGS[@]}"; do
        case "$DISTRO" in
            fedora) run $SUDO dnf install -y "$p" ;;
            arch)   run $SUDO pacman -S --needed --noconfirm "$p" ;;
            debian) run $SUDO apt-get install -y "$p" ;;
            suse)   run $SUDO zypper --non-interactive install "$p" ;;
        esac || echo "    (could not install $p; the Windows page will tell you if it needs it)"
    done
}

install_hint() {
    case "$DISTRO" in
        fedora) echo "sudo dnf install parted dosfstools udisks2 polkit util-linux" ;;
        arch)   echo "sudo pacman -S parted dosfstools udisks2 polkit util-linux" ;;
        debian) echo "sudo apt install parted dosfstools udisks2 pkexec util-linux" ;;
        suse)   echo "sudo zypper install parted dosfstools udisks2 polkit util-linux" ;;
        *)      echo "install parted, dosfstools, udisks2, polkit (pkexec) and util-linux" ;;
    esac
}

# ----------------------------------------------------------- apt helpers
apt_pick() {   # "a|b|c" -> first alternative that has an install candidate
    local alt cand first="${1%%|*}"
    if ! command -v apt-cache >/dev/null 2>&1; then echo "$first"; return; fi
    for alt in ${1//|/ }; do
        cand="$(apt-cache policy "$alt" 2>/dev/null | awk '/Candidate:/ {print $2}')"
        if [ -n "$cand" ] && [ "$cand" != "(none)" ]; then echo "$alt"; return; fi
    done
    if [ "$DRY_RUN" = "1" ]; then echo "$first"; fi   # package lists may be empty in a dry run
}

resolve_apt() {
    local p r
    for p in "$@"; do
        r="$(apt_pick "$p")"
        if [ -n "$r" ]; then
            echo "$r"
        else
            echo "WARNING: no package found for '${p//|/ or }' in your repositories (skipped)" >&2
        fi
    done | awk '!seen[$0]++'
}

# Newest N (>= MIN_CARGO_MINOR) for which both rustc-1.N and cargo-1.N exist.
apt_versioned_rust() {
    local best="" p n
    for p in $(apt-cache pkgnames rustc-1. 2>/dev/null); do
        n="${p#rustc-1.}"
        case "$n" in ''|*[!0-9]*) continue ;; esac
        [ "$n" -ge "$MIN_CARGO_MINOR" ] || continue
        apt-cache pkgnames "cargo-1.$n" 2>/dev/null | grep -qx "cargo-1.$n" || continue
        if [ -z "$best" ] || [ "$n" -gt "$best" ]; then best="$n"; fi
    done
    echo "$best"
}

# ----------------------------------------------------------- 1. packages
if [ "$INSTALL_DEPS" = "1" ]; then
    echo "==> Detected distribution family: $DISTRO"
    case "$DISTRO" in
        fedora)
            echo "==> Installing required packages with dnf"
            run $SUDO dnf install -y "${PKGS[@]}"
            ;;
        arch)
            PKG_LIST=("${PKGS[@]}")
            if command -v pacman >/dev/null 2>&1 && pacman -Qq rustup >/dev/null 2>&1; then
                echo "==> rustup is installed: not installing the 'rust' package (they conflict)."
                echo "    Make sure a toolchain is set:  rustup default stable"
                PKG_LIST=("${PKG_LIST[@]:1}")
            fi
            echo "==> Installing required packages with pacman"
            run $SUDO pacman -S --needed --noconfirm "${PKG_LIST[@]}"
            ;;
        debian)
            echo "==> Installing required packages with apt"
            run $SUDO apt-get update
            mapfile -t APT_PKGS < <(resolve_apt "${PKGS[@]}")
            run $SUDO apt-get install -y "${APT_PKGS[@]}"
            ;;
        suse)
            echo "==> Installing required packages with zypper"
            run $SUDO zypper --non-interactive install "${PKGS[@]}"
            ;;
        *)
            echo "Could not detect dnf, pacman, apt or zypper on this system." >&2
            echo "Install these (names may differ), then run: ./install.sh --no-deps" >&2
            echo "  Rust 1.85+ and Cargo, a C compiler, pkg-config," >&2
            echo "  fontconfig, libxkbcommon and wayland development files," >&2
            echo "  Vulkan loader + Mesa, xdg-desktop-portal, polkit (pkexec)," >&2
            echo "  udisks2, parted, dosfstools, util-linux" >&2
            exit 1
            ;;
    esac
    install_win_pkgs
fi

# ---------------------------------------------------------------- 2. build
cargo_minor() {
    { "$@" --version 2>/dev/null || true; } | sed -n 's/^cargo 1\.\([0-9][0-9]*\)\..*/\1/p'
}

CARGO_CMD=()
try_dir() {   # $1 = a directory holding cargo and rustc
    local d="$1" m
    { [ -x "$d/cargo" ] && [ -x "$d/rustc" ]; } || return 1
    m="$(cargo_minor "$d/cargo")"
    { [ -n "$m" ] && [ "$m" -ge "$MIN_CARGO_MINOR" ]; } || return 1
    CARGO_CMD=(env "PATH=$d:$PATH" "RUSTC=$d/rustc" "$d/cargo")
}

# An old cargo from rustup in ~/.cargo/bin can shadow the newer system one, and
# Debian/Ubuntu keep newer toolchains in /usr/lib/rust-1.N/bin.
select_cargo() {
    local v d
    v="$(cargo_minor cargo)"
    if [ -n "$v" ] && [ "$v" -ge "$MIN_CARGO_MINOR" ]; then
        CARGO_CMD=(cargo)
        return 0
    fi
    for d in "$SYS_BIN" $(compgen -G "$RUST_DIRS_GLOB" | sort -Vr); do
        if try_dir "$d"; then
            echo "==> The 'cargo' in your PATH is too old ($(cargo --version 2>/dev/null || echo missing)); using $d/cargo ($("$d/cargo" --version))"
            return 0
        fi
    done
    return 1
}

if ! select_cargo; then
    if [ "$DISTRO" = "debian" ] && [ "$INSTALL_DEPS" = "1" ] && [ "$DRY_RUN" != "1" ] \
       && command -v apt-cache >/dev/null 2>&1; then
        n="$(apt_versioned_rust)"
        if [ -n "$n" ]; then
            echo "==> Installing a newer Rust from your distribution: rustc-1.$n cargo-1.$n"
            $SUDO apt-get install -y "rustc-1.$n" "cargo-1.$n"
            select_cargo || true
        fi
    fi
fi

if [ "${#CARGO_CMD[@]}" -eq 0 ]; then
    if [ "$DRY_RUN" = "1" ]; then
        echo "NOTE: no Cargo 1.85+ found (dry run, continuing)"
        CARGO_CMD=(cargo)
    else
        echo "ERROR: Cargo 1.85 or newer is required (found: $(cargo --version 2>/dev/null || echo none))." >&2
        echo "       Fix: $RUST_HINT" >&2
        exit 1
    fi
fi

echo "==> Building (release)"
run "${BUILD_AS[@]}" "${CARGO_CMD[@]}" build --release

# -------------------------------------------------------------- 3. install
echo "==> Installing to $PREFIX"
run $SUDO install -Dm755 target/release/ignite "$PREFIX/bin/ignite"

DESKTOP_TMP="$(mktemp)"
trap 'rm -f "$DESKTOP_TMP"' EXIT
sed "s|^Exec=ignite\$|Exec=$PREFIX/bin/ignite|; s|^Exec=ignite --compact\$|Exec=$PREFIX/bin/ignite --compact|" ignite.desktop > "$DESKTOP_TMP"
run $SUDO install -Dm644 "$DESKTOP_TMP" "$PREFIX/share/applications/ignite.desktop"

for size in 512 256 128 64 48 32; do
    run $SUDO install -Dm644 "data/icons/hicolor/${size}x${size}/apps/ignite.png" \
        "$PREFIX/share/icons/hicolor/${size}x${size}/apps/ignite.png"
done
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    run $SUDO gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" || true
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    run $SUDO update-desktop-database "$PREFIX/share/applications" || true
fi

# ------------------------------------------------------------ 4. sanity check
missing=()
for c in "${RUNTIME_CMDS[@]}"; do
    command -v "$c" >/dev/null 2>&1 || missing+=("$c")
done
if [ "${#missing[@]}" -gt 0 ] && [ "$DRY_RUN" != "1" ]; then
    echo "WARNING: these commands are still missing: ${missing[*]}" >&2
    echo "         install: $(install_hint)" >&2
fi

echo "==> Done. Start it from the application menu (Ignite) or run: $PREFIX/bin/ignite"
