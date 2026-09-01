#!/bin/sh
set -eu

i=0
while [ "$i" -lt 12000 ]; do
    printf 'stdout-%s\n' "$i"
    i=$((i + 1))
done
printf '\033[32mFINAL-STDERR\033[0m\n' >&2
