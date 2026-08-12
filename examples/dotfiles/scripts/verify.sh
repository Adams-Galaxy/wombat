#!/bin/sh

set -eu

for argument in "$@"; do
    case "$argument" in
        --target-root=*) target_root=${argument#*=} ;;
    esac
done

test -n "${target_root:-}"
test -f "$target_root/.wombat-example"
printf '%s\n' "canonical example deployment verified"
