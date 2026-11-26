#!/usr/bin/env bash
# Builds a game (or an engine example) for the browser and assembles
# dist/web/<name>/ with index.html, <name>.js, <name>.wasm and <name>.data
# (the game's asset folders, preloaded into the page's virtual file system
# at /assets, /profiles, ...). See docs/WEB.md.
#
# Usage: ./package_web.sh <game>            games/<game>, release build
#        ./package_web.sh --example <name>  engine/examples/<name>.rs
#        ./package_web.sh ... --serve       then serve on http://localhost:8000
#
# Needs the emscripten SDK on PATH (`source emsdk_env.sh`) and
# `rustup target add wasm32-unknown-emscripten`.
set -euo pipefail

usage() {
    echo "Usage: $0 <game> | --example <name> [--serve]" >&2
    exit 2
}

name=""
example=0
serve=0
while [ $# -gt 0 ]; do
    case "$1" in
        --example) example=1; shift; [ $# -gt 0 ] || usage; name="$1" ;;
        --serve) serve=1 ;;
        -h|--help) usage ;;
        -*) echo "error: unknown option '$1'" >&2; usage ;;
        *) [ -z "$name" ] || usage; name="$1" ;;
    esac
    shift
done
[ -n "$name" ] || usage

command -v emcc >/dev/null || { echo "error: emcc not found; source emsdk_env.sh first" >&2; exit 1; }

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target=wasm32-unknown-emscripten
target_dir="${CARGO_TARGET_DIR:-$root/target}"
dist="$root/dist/web/$name"

# Asset folders a game ships (same list as package.sh), preloaded at /<dir>.
preload=()
if [ "$example" -eq 0 ]; then
    game_dir="$root/games/$name"
    [ -d "$game_dir" ] || { echo "error: No such game: games/$name" >&2; exit 1; }
    for d in assets profiles levels rigs classes scripts sfx music; do
        if [ -d "$game_dir/$d" ]; then
            preload+=("-C" "link-arg=--preload-file=$game_dir/$d@/$d")
        fi
    done
fi

cd "$root"
if [ "$example" -eq 1 ]; then
    echo "Building example '$name' for the web..."
    cargo rustc --release --target "$target" -p dust_engine --example "$name" -- "${preload[@]}"
    out="$target_dir/$target/release/examples"
else
    echo "Building '$name' for the web..."
    cargo rustc --release --target "$target" -p "$name" --bin "$name" -- "${preload[@]}"
    out="$target_dir/$target/release"
fi

rm -rf "${dist:?}"
mkdir -p "$dist"
cp "$out/$name.js" "$out/$name.wasm" "$dist/"
[ -f "$out/$name.data" ] && cp "$out/$name.data" "$dist/"
sed -e "s/__TITLE__/$name/" -e "s/__GAME__/$name/" "$root/web/index.html" > "$dist/index.html"
echo "Packaged $dist"
ls -la "$dist"

if [ "$serve" -eq 1 ]; then
    echo "Serving on http://localhost:8000 (Ctrl+C to stop)"
    cd "$dist" && python3 -m http.server 8000
fi
