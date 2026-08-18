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
            if have apt-get; then
                packages="ca-certificates curl"
                have git || packages="$packages git"
                have cc || packages="$packages build-essential"
                elevated apt-get update
                # shellcheck disable=SC2086 # Every word is selected from the fixed package list above.
                elevated env DEBIAN_FRONTEND=noninteractive apt-get install --yes $packages
            elif have dnf; then
                # Fedora's split build tool packages are cheap and deterministic;
                # installing the complete prerequisite layer avoids a half-ready
                # Rust build environment after this explicit authorization.
                elevated dnf install --assumeyes ca-certificates curl git gcc make
            else
                printf '%s\n' "Automatic prerequisite installation supports Debian-family and Fedora Linux." >&2
                exit 1
            fi
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
cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
cargo_default_root=${CARGO_INSTALL_ROOT:-"$cargo_home"}
if [ -n "${WOMBAT_INSTALL_REV:-}" ]; then
    "$cargo_path" install --git "$repository" --rev "$WOMBAT_INSTALL_REV" --locked --force --root "$install_root" wombat
else
    "$cargo_path" install --git "$repository" --branch main --locked --force --root "$install_root" wombat
fi

installed_binary="$install_root/bin/wombat"
default_binary="$cargo_default_root/bin/wombat"
if [ "$default_binary" != "$installed_binary" ] && [ -x "$default_binary" ]; then
    printf '%s\n' "warning: another Wombat executable exists at $default_binary; this installer uses $installed_binary." >&2
    printf '%s\n' "warning: remove or update the other executable so PATH cannot select an older Wombat." >&2
fi

printf '%s\n' "Installed Wombat at $installed_binary." >&2
printf '%s\n' "To update this installation, use:" >&2
if [ -n "${WOMBAT_INSTALL_REV:-}" ]; then
    printf '  %s install --git %s --rev %s --locked --force --root %s wombat\n' \
        "$cargo_path" "$repository" "$WOMBAT_INSTALL_REV" "$install_root" >&2
else
    printf '  %s install --git %s --branch main --locked --force --root %s wombat\n' \
        "$cargo_path" "$repository" "$install_root" >&2
fi

exec "$installed_binary" "$@"
