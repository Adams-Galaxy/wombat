#!/bin/sh

set -eu

if ! command -v orb >/dev/null 2>&1; then
    printf '%s\n' "OrbStack's orb command is required." >&2
    exit 1
fi
if ! command -v rsync >/dev/null 2>&1; then
    printf '%s\n' "rsync is required to snapshot the working tree." >&2
    exit 1
fi

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
machine=${WOMBAT_PROVING_MACHINE:-wombat-plan0009}
case "$machine" in
    *[!a-zA-Z0-9_-]*|'')
        printf '%s\n' "WOMBAT_PROVING_MACHINE must contain only letters, numbers, _ or -." >&2
        exit 1
        ;;
esac
if orb list | awk '{print $1}' | grep -Fx "$machine" >/dev/null 2>&1; then
    printf '%s\n' "OrbStack machine $machine already exists; choose a fresh WOMBAT_PROVING_MACHINE." >&2
    exit 1
fi

proof_root=$(mktemp -d "$repository_root/target/plan0009-proof.XXXXXX")
snapshot="$proof_root/wombat-source"
rsync -a --exclude .git --exclude target "$repository_root/" "$snapshot/"
git -C "$snapshot" init -b main >/dev/null
git -C "$snapshot" config user.name "Wombat Proving Run"
git -C "$snapshot" config user.email wombat@example.invalid
git -C "$snapshot" add .
git -C "$snapshot" commit -m "Plan 0009 proving snapshot" >/dev/null
git clone --bare "$snapshot" "$proof_root/wombat.git" >/dev/null

example="$proof_root/example-source"
mkdir -p "$example"
rsync -a --exclude target "$snapshot/examples/dotfiles/" "$example/"
git -C "$example" init -b main >/dev/null
git -C "$example" config user.name "Wombat Proving Run"
git -C "$example" config user.email wombat@example.invalid
git -C "$example" add .
git -C "$example" commit -m "Canonical example" >/dev/null
git clone --bare "$example" "$proof_root/example.git" >/dev/null

linux_proof_root="/mnt/mac$proof_root"
orb create --user wombat ubuntu:24.04 "$machine"

orb -m "$machine" env \
    WOMBAT_INSTALL_REPOSITORY="file://$linux_proof_root/wombat.git" \
    sh "$linux_proof_root/wombat-source/install.sh" \
    --install-prerequisites setup "file://$linux_proof_root/example.git" --yes

orb -m "$machine" /home/wombat/.local/bin/wombat \
    setup "file://$linux_proof_root/example.git" --yes
orb -m "$machine" sh -c '
    set -eu
    test -f "$HOME/.gitconfig"
    test -f "$HOME/.config/wombat-editor.toml"
    test -x "$HOME/.local/wombat-tools/bin/wombat-info"
    rg --version | head -1
    "$HOME/.local/wombat-tools/bin/wombat-info"
'

printf '%s\n' "Plan 0009 proving run completed in OrbStack machine $machine."
printf '%s\n' "Inspect it with: orb -m $machine"
printf '%s\n' "Remove it when finished with: orb delete $machine"
