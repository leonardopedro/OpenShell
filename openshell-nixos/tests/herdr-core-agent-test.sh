#!/usr/bin/env bash
# Fixture tests for the runner's status reports. No hc or VM is started.
set -uo pipefail
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FAILED=0
pass() { printf 'ok - %s\n' "$1"; }
fail() { printf 'not ok - %s\n' "$1" >&2; FAILED=1; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; fi; }

out="$("$ROOT/herdr-core-agent" __runner t sh -c 'exit 0' 2>&1)"
check "success reports idle then done" '[[ "$out" == *"state=idle"*"state=done"* ]]'
check "success contains no error" '[[ "$out" != *"state=error"* ]]'

out="$("$ROOT/herdr-core-agent" __runner t sh -c 'exit 7' 2>&1)"
rc=$?
check "failure preserves the child status" '[[ "$rc" == 7 ]]'
check "failure reports error, not done" '[[ "$out" == *"state=idle"*"state=error"* ]]'
check "failure never reports done" '[[ "$out" != *"state=done"* ]]'

out="$("$ROOT/herdr-core-agent" __runner t sh -c 'kill -TERM $$' 2>&1)"
rc=$?
check "signal exit is nonzero" '[[ "$rc" != 0 ]]'
check "signal reports error" '[[ "$out" == *"state=error"* ]]'

if (( FAILED )); then
  printf 'herdr-core-agent tests failed\n' >&2
  exit 1
fi
printf 'herdr-core-agent tests passed\n'
