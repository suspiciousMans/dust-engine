#!/usr/bin/env bash
# Build a Dust game for the browser and stage it in dist-web/<name>/.
# usage: web/build.sh <package> [example]     (needs emsdk on PATH: `source emsdk_env.sh`)
# Assets: set DUST_PRELOAD="assets levels ..." to bundle those folders into a
# .data file (relative to the package directory) via --preload-file.
set -euo pipefail
pkg=${1:?package name}; ex=${2:-}
root=$(cd "$(dirname "$0")/.." && pwd); cd "$root"
command -v emcc >/dev/null || { echo "emcc not found: source emsdk_env.sh first" >&2; exit 1; }
flags=""
for d in ${DUST_PRELOAD:-}; do flags="$flags -sFORCE_FILESYSTEM=1 --preload-file $root/$( [ -d "games/$pkg/$d" ] && echo games/$pkg || echo . )/$d@/$d"; done
# EMCC_CFLAGS is read by emcc itself, so config.toml rustflags stay in effect.
export EMCC_CFLAGS="${EMCC_CFLAGS:-} $flags"
if [ -n "$ex" ]; then cargo build --release -p "$pkg" --example "$ex" --target wasm32-unknown-emscripten; out=target/wasm32-unknown-emscripten/release/examples; name=$ex
else cargo build --release -p "$pkg" --target wasm32-unknown-emscripten; out=target/wasm32-unknown-emscripten/release; name=$pkg; fi
dest=dist-web/$name; mkdir -p "$dest"
cp web/index.html "$dest/"; cp "$out/$name".{js,wasm} "$dest/"; cp "$out/$name".data "$dest/" 2>/dev/null || true
echo "built $dest  (serve it: python3 -m http.server -d $dest; open /index.html?app=$name)"
