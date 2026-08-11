#!/bin/sh

set -eu

test "$#" -eq 4
rg --version >/dev/null
printf '%s\n' "canonical example validation passed"
