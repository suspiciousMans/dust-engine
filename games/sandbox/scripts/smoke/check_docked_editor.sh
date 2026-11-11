#!/bin/sh
# Runs docked_editor.pss and checks the open/close log lines it documents.
#   games/sandbox/scripts/smoke/check_docked_editor.sh   (from the repo root, after cargo build -p sandbox)
set -eu
cd "$(dirname "$0")/../.."
out=$(RUST_LOG=info timeout 90 ../../target/debug/sandbox --smoke-test scripts/smoke/docked_editor.pss 2>&1) || { echo "$out" | tail -20; echo FAIL: smoke test failed; exit 1; }
seq=$(echo "$out" | grep -o 'docked editor \(opened\|closed\)' | tr '\n' ',')
want='docked editor opened,docked editor closed,docked editor opened,docked editor closed,docked editor opened,docked editor closed,'
[ "$seq" = "$want" ] || { echo "FAIL: got $seq"; exit 1; }
echo "$out" | grep -q 'docked editor smoke test passed' || { echo FAIL: no pass line; exit 1; }
echo "ok: F2, F1, F4 open/close the dock"
