#!/usr/bin/env bash
# Builds a game in release mode and assembles a distributable folder under
# dist/<game>/. POSIX/bash equivalent of package.ps1.
#
# Usage: ./package.sh <game> [--skip-build]
#   --skip-build   reuse an existing release binary instead of running cargo
#
# Release builds automatically strip the dev editor (the games' own
# editor_available() returns cfg!(debug_assertions)), so a packaged game
# boots straight into Play mode - no extra flags needed here.
set -euo pipefail

usage() {
    echo "Usage: $0 <game> [--skip-build]" >&2
    exit 2
}

game=""
build=1
for arg in "$@"; do
    case "$arg" in
        --skip-build) build=0 ;;
        -h|--help) usage ;;
        -*) echo "error: unknown option '$arg'" >&2; usage ;;
        *)
            [ -z "$game" ] || usage
            game="$arg"
            ;;
    esac
done
[ -n "$game" ] || usage

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
game_dir="$root/games/$game"

if [ ! -d "$game_dir" ]; then
    echo "error: No such game: games/$game" >&2
    exit 1
fi

if [ "$build" -eq 1 ]; then
    echo "Building '$game' in release mode..."
    (cd "$root" && cargo build --release -p "$game")
fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
exe_path=""
for candidate in "$target_dir/release/$game" "$target_dir/release/$game.exe"; do
    if [ -f "$candidate" ]; then
        exe_path="$candidate"
        break
    fi
done
if [ -z "$exe_path" ]; then
    echo "error: Expected build output not found: $target_dir/release/$game" >&2
    exit 1
fi

dist_dir="$root/dist/$game"
rm -rf "$dist_dir"
mkdir -p "$dist_dir"

cp "$exe_path" "$dist_dir/"

# Every folder a game's asset root can hold, except "saves" - checkpoints
# are pure runtime state (re-created empty on first launch, see
# Sandbox::init), so shipping the dev's own checkpoint.ron would just hand
# players a stale position instead of a fresh game.
for folder in assets profiles levels rigs classes scripts sfx music; do
    if [ -d "$game_dir/$folder" ]; then
        cp -R "$game_dir/$folder" "$dist_dir/$folder"
    fi
done

# Script build caches (.ox-build) are machine-specific; the game rebuilds its
# Oxidized scripts on first run (needs `oxidized` + `rustc`, see
# docs/SCRIPTING-OXIDIZED.md section 7).
find "$dist_dir" -name .ox-build -type d -prune -exec rm -rf {} +

echo "Packaged to $dist_dir"
(cd "$dist_dir" && find . -mindepth 1 | sed 's|^\./|  |' | sort)
