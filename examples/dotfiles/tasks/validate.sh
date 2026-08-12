#!/bin/sh

set -eu

test "$#" -eq 6
rg --version >/dev/null
printf '%s\n' "canonical example validation passed"
