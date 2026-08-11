#!/bin/sh

set -eu

install_prerequisites=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --install-prerequisites)
            install_prerequisites=true
            shift
            ;;
        --)
            shift
            break
            ;;
        *)
            break
            ;;
    esac
done

if [ "$#" -eq 0 ]; then
    printf '%s\n' "usage: install.sh [--install-prerequisites] setup REPOSITORY [OPTIONS]" >&2
    exit 2
fi

have() {
    command -v "$1" >/dev/null 2>&1
}

missing=""
for command_name in git cc cargo curl; do
    if ! have "$command_name"; then
        missing="${missing}${missing:+ }${command_name}"
    fi
done

if [ -n "$missing" ]; then
    printf '%s\n' "Wombat needs these development prerequisites before it can be installed: $missing" >&2
    if [ "$install_prerequisites" != true ]; then
        if [ -t 0 ]; then
            printf '%s' "Install the missing prerequisites now? [y/N] " >&2
            read -r answer
            case "$answer" in
                y|Y|yes|YES) install_prerequisites=true ;;
                *) printf '%s\n' "Installation cancelled without changing prerequisites." >&2; exit 1 ;;
            esac
        else
            printf '%s\n' "Rerun with --install-prerequisites to authorize this prerequisite layer." >&2
            exit 1
        fi
    fi
fi

elevated() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    elif have sudo; then
        sudo -- "$@"
    else
        printf '%s\n' "Installing prerequisites requires root or sudo." >&2
        exit 1
    fi
}

system_name=$(uname -s)
if [ -n "$missing" ] && [ "$install_prerequisites" = true ]; then
    case "$system_name" in
        Linux)
            if ! have apt-get; then
                printf '%s\n' "Automatic prerequisite installation currently supports Debian-family Linux only." >&2
                exit 1
            fi
            packages="ca-certificates curl"
            have git || packages="$packages git"
            have cc || packages="$packages build-essential"
            elevated apt-get update
            elevated env DEBIAN_FRONTEND=noninteractive apt-get install --yes $packages
            ;;
        Darwin)
            if ! have git || ! have cc; then
                printf '%s\n' "Starting Apple's Command Line Tools installer. Complete it, then rerun this command." >&2
                xcode-select --install || true
                exit 1
            fi
            ;;
        *)
            printf '%s\n' "Automatic prerequisite installation is unsupported on $system_name." >&2
            exit 1
            ;;
    esac
fi

if ! have cargo; then
    if ! have curl; then
        printf '%s\n' "curl is required to install Rustup." >&2
        exit 1
    fi
    rustup_script=$(mktemp "${TMPDIR:-/tmp}/wombat-rustup.XXXXXX")
    trap 'rm -f "$rustup_script"' EXIT HUP INT TERM
    curl -fsSL https://sh.rustup.rs -o "$rustup_script"
    sh "$rustup_script" -y --profile minimal --default-toolchain stable
    cargo_path="$HOME/.cargo/bin/cargo"
else
    cargo_path=$(command -v cargo)
fi

install_root=${WOMBAT_INSTALL_ROOT:-"$HOME/.local"}
repository=${WOMBAT_INSTALL_REPOSITORY:-"https://github.com/Adams-Galaxy/wombat.git"}
if [ -n "${WOMBAT_INSTALL_REV:-}" ]; then
    "$cargo_path" install --git "$repository" --rev "$WOMBAT_INSTALL_REV" --locked --force --root "$install_root" wombat
else
    "$cargo_path" install --git "$repository" --branch main --locked --force --root "$install_root" wombat
fi

exec "$install_root/bin/wombat" "$@"
