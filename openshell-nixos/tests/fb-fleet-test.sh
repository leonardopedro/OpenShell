#!/usr/bin/env bash
# Stub-driven acceptance for fb-fleet's plan/apply safety.
set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fb-fleet-test.XXXXXX")"
trap '[[ "${KEEP_TMP:-0}" == 1 ]] || rm -rf "$TMP"' EXIT
BIN="$TMP/bin"
STATE="$TMP/stub-state"
mkdir -p "$BIN" "$STATE"
FAILED=0

pass() { printf 'ok - %s\n' "$1"; }
fail() { printf 'not ok - %s\n' "$1" >&2; FAILED=1; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; fi; }

cat > "$TMP/policy.yaml" <<'EOF'
version: 1
network_policies:
  freebuff:
    binaries:
      - path: /workspace/fb-a/.config/manicode/freebuff
      - path: /workspace/fb-b/.config/manicode/freebuff
EOF
printf '#!/bin/sh\nexit 0\n' > "$TMP/freebuff"
chmod +x "$TMP/freebuff"
mkdir -p "$TMP/infinity"
for bin in infinity infinity-provider-openai sandbox-local; do
  printf '#!/bin/sh\nexit 0\n' > "$TMP/infinity/$bin"
  chmod +x "$TMP/infinity/$bin"
done

cat > "$BIN/openshell" <<'EOF'
#!/usr/bin/env bash
set -u
echo "openshell $*" >> "$STUB_LOG"
if [[ "$1 $2" == "sandbox get" ]]; then
  [[ -f "$STUB_STATE/vm" ]] || exit 1
  cat "$STUB_STATE/vm"
  exit 0
fi
if [[ "$1 $2" == "sandbox create" ]]; then
  [[ "${STUB_FAIL_CREATE:-0}" == 1 ]] && exit 1
  mounts="/nix:/nix:ro,$FLEET_DIR:/workspace:rw"
  sha="$(sha256sum "$POLICY" | cut -d' ' -f1)"
  state="${STUB_CREATE_STATE:-ready}"
  printf 'name=fleet id=vm-1 state=%s provider=edge0-gpu policy_sha=%s mounts=%s\n' \
    "$state" "$sha" "$mounts" > "$STUB_STATE/vm"
  exit 0
fi
exit 2
EOF

cat > "$BIN/hc" <<'EOF'
#!/usr/bin/env bash
set -u
echo "hc $*" >> "$STUB_LOG"
if [[ "$1" == ls ]]; then
  [[ -f "$STUB_STATE/terminals" ]] && cat "$STUB_STATE/terminals"
  exit 0
fi
if [[ "$1" == run ]]; then
  name=""
  while (( $# )); do
    case "$1" in
      --name) name="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  echo "$name" >> "$STUB_STATE/terminals"
  [[ "${STUB_HC_FAIL_AFTER_START:-0}" == 1 ]] && exit 1
  exit 0
fi
exit 2
EOF
chmod +x "$BIN/openshell" "$BIN/hc"

export STUB_STATE="$STATE" STUB_LOG="$TMP/stub.log" FLEET_DIR="$TMP/fleet"
export POLICY="$TMP/policy.yaml" FREEBUFF_BIN="$TMP/freebuff"
export OSH="$BIN/openshell" HC="$BIN/hc" INFINITY_BIN_DIR="$TMP/infinity"
export FB_SESSIONS=fb-a,fb-b INF_SESSIONS=
export PROVIDER=edge0-gpu READY_TIMEOUT=2
export STUB_FAIL_CREATE=0 STUB_CREATE_STATE=ready STUB_HC_FAIL_AFTER_START=0

calls() { grep -c "$1" "$STUB_LOG" 2>/dev/null || true; }
apply() {
  "$ROOT/fb-fleet" --apply >"$TMP/out" 2>"$TMP/err"
  local rc=$?
  (( rc == 0 )) || cat "$TMP/err" >&2
  return $rc
}

# 1. Plan is the default and performs zero writes.
rm -f "$STUB_LOG"
"$ROOT/fb-fleet" --plan --json > "$TMP/plan.json"
check "plan leaves no fleet directory" '[[ ! -e "$FLEET_DIR" ]]'
check "plan invokes neither CLI" '[[ ! -s "$STUB_LOG" ]]'
check "plan is valid JSON" 'jq -e ".vm.action == \"create\" and (.sessions|length) == 2" "$TMP/plan.json" >/dev/null'

# 2. First apply creates one VM and starts each terminal once.
STUB_FAIL_CREATE=0; STUB_CREATE_STATE=ready; STUB_HC_FAIL_AFTER_START=0
apply
check "first apply creates exactly one VM" '[[ "$(calls "sandbox create")" == 1 ]]'
check "first apply starts two terminals" '[[ "$(calls "hc run")" == 2 ]]'
check "first apply records running sessions" '[[ "$(cat "$FLEET_DIR/.fleet/fb-a.state" | grep -c status=running)" == 1 ]]'

# 3. Repeat apply reuses the VM and active terminals.
before_create="$(calls "sandbox create")"
before_run="$(calls "hc run")"
apply
check "repeat apply does not create another VM" '[[ "$(calls "sandbox create")" == "$before_create" ]]'
check "repeat apply does not duplicate terminals" '[[ "$(calls "hc run")" == "$before_run" ]]'

# 4. A stale policy fails before launching anything.
cp "$POLICY" "$TMP/policy.good"
grep -v 'fb-b/.config' "$POLICY" > "$TMP/policy.stale"
cp "$TMP/policy.stale" "$POLICY"
before_run="$(calls "hc run")"
check "stale policy rejects apply" '! apply'
check "stale policy launches no terminal" '[[ "$(calls "hc run")" == "$before_run" ]]'
cp "$TMP/policy.good" "$POLICY"

# 5. Wrong VM ID is rejected.
cp "$FLEET_DIR/.fleet/vm.env" "$TMP/vm.good"
sed 's/^id=.*/id=vm-2/' "$TMP/vm.good" > "$STUB_STATE/vm"
before_run="$(calls "hc run")"
check "wrong VM ID rejects apply" '! apply'
check "wrong VM ID launches no terminal" '[[ "$(calls "hc run")" == "$before_run" ]]'
cp "$TMP/vm.good" "$STUB_STATE/vm"

# 6. Failed create never reaches terminals.
fresh() { rm -rf "$TMP/fleet" "$STUB_STATE"; mkdir -p "$STUB_STATE"; rm -f "$STUB_LOG"; }
fresh
STUB_FAIL_CREATE=1; STUB_CREATE_STATE=ready
check "failed create exits nonzero" '! apply'
check "failed create launches no terminal" '[[ "$(calls "hc run")" == 0 ]]'
STUB_FAIL_CREATE=0

# 7. A VM that never becomes Ready never reaches terminals.
fresh
STUB_CREATE_STATE=starting
check "not-Ready exits nonzero" '! apply'
check "not-Ready launches no terminal" '[[ "$(calls "hc run")" == 0 ]]'
STUB_CREATE_STATE=ready

# 8. Uncertain launch is reconciled by name, not repeated.
fresh
STUB_HC_FAIL_AFTER_START=1
check "lost launch reply exits nonzero" '! apply'
check "lost launch reply records uncertain" 'grep -q status=uncertain "$FLEET_DIR/.fleet/fb-a.state"'
STUB_HC_FAIL_AFTER_START=0
before_run="$(calls "hc run")"
apply
check "retry starts only the terminal that was missing" '[[ "$(calls "hc run")" == "$((before_run + 1))" ]]'
check "retry marks the session running" 'grep -q status=running "$FLEET_DIR/.fleet/fb-a.state"'

# 9. A record left in launching state without a terminal retries once.
fresh
apply
mkdir -p "$FLEET_DIR/.fleet"
sed 's/^status=.*/status=launching/' "$FLEET_DIR/.fleet/fb-a.state" > "$TMP/state"
mv "$TMP/state" "$FLEET_DIR/.fleet/fb-a.state"
printf 'fb-b\n' > "$STUB_STATE/terminals"
before_run="$(calls "hc run")"
apply
check "interrupted launching state retries only the missing terminal" '[[ "$(calls "hc run")" == "$((before_run + 1))" ]]'

if (( FAILED )); then
  printf 'fb-fleet tests failed\n' >&2
  exit 1
fi
printf 'fb-fleet tests passed\n'
