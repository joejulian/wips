#!/bin/sh

set -eu

find_wips() {
    if command -v wips >/dev/null 2>&1; then
        command -v wips
        return
    fi

    cargo_home=${CARGO_HOME:-${HOME:-}/.cargo}
    if [ -n "$cargo_home" ] && [ -x "$cargo_home/bin/wips" ]; then
        printf '%s\n' "$cargo_home/bin/wips"
        return
    fi

    return 1
}

if wips_path=$(find_wips); then
    "$wips_path" --version >&2
    printf '%s\n' "$wips_path"
    exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' "wips is not installed and Cargo is unavailable" >&2
    exit 1
fi

cargo install --git https://github.com/joejulian/wips.git --locked

if ! wips_path=$(find_wips); then
    printf '%s\n' "Cargo completed but the installed wips executable could not be found" >&2
    exit 1
fi

"$wips_path" --version >&2
printf '%s\n' "$wips_path"
