#!/usr/bin/env bash
# Builds and tests the server in one go, with the steps of the `server` job in CI:
# format check, clippy, tests, the check that the server does not depend on GPUI, and a
# release build of bunsind.
#
# Usage: scripts/check.sh [options]
#   --fix        Format the code (cargo fmt) instead of only checking it.
#   --with-app   Also check the desktop window (bunsin-app), as the `app` job in CI does. macOS only.
#   --no-build   Skip the release build of bunsind at the end.
#   -h, --help   Show this help.
#
# Each step prints its command. The script stops at the first step that fails.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"

usage() { awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"; }

fix=false
with_app=false
build=true
while [ $# -gt 0 ]; do
    case "$1" in
        --fix) fix=true ;;
        --with-app) with_app=true ;;
        --no-build) build=false ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

if $with_app && [ "$(uname -s)" != Darwin ]; then
    echo "--with-app needs macOS: the desktop window uses GPUI." >&2
    exit 2
fi

# The workspace without the window, or the whole workspace.
scope=(--workspace --exclude bunsin-app)
$with_app && scope=(--workspace)

step=0
started=$SECONDS
run() {
    step=$((step + 1))
    printf '\n\033[1m==> [%d] %s\033[0m\n' "$step" "$*"
    "$@"
}

if $fix; then
    run cargo fmt --all
else
    run cargo fmt --all --check
fi
run cargo clippy "${scope[@]}" --all-targets --locked -- -D warnings
run cargo test "${scope[@]}" --locked

step=$((step + 1))
printf '\n\033[1m==> [%d] the server must not depend on GPUI\033[0m\n' "$step"
if cargo tree -p bunsin-daemon --locked | grep -i gpui; then
    echo "bunsin-daemon depends on GPUI" >&2
    exit 1
fi
echo "ok"

if $build; then
    run cargo build --release -p bunsin-daemon --locked
    echo "built: target/release/bunsind"
fi

printf '\n\033[1;32mAll checks passed\033[0m in %ds.\n' "$((SECONDS - started))"
