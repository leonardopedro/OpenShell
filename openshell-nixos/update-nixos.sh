#!/usr/bin/env bash
# update-nixos.sh — re-render /etc/nixos/openshell.nix from this folder's
# nixos-module.nix template and apply it. Run as sudo:
#
#   sudo ./update-nixos.sh
#
# Why this exists: the module pins this folder as a locked flake ref
# (builtins.getFlake "path:<folder>?narHash=..."). When the folder moves
# or its contents change, the pinned path/hash goes stale — the rendered
# file can point at a path that no longer exists, which makes the NEXT
# nixos-rebuild fail at eval. install.sh does the same re-render, but it
# also restarts the gateway and touches user config; this script only
# fixes /etc/nixos, so it is safe to run whenever the folder changes.
#
# What it does:
#   1. preflight  — root, template exists, module still carries the
#                   managed-by marker (hand-managed files are left alone)
#   2. relock     — nix flake prefetch of THIS folder (fresh narHash; the
#                   folder is its own git repo and every change is
#                   committed, so the hash is stable between edits)
#   3. render     — @OPENSHELL_REF@ (new path + hash), @USER@ (sudo caller),
#                   @KSM_ENABLED@ (kept as found in the current module,
#                   default true), kvm group line kept iff the group exists
#   4. track      — git add openshell.nix (untracked files are invisible
#                   to flake eval in a git worktree)
#   5. switch     — nixos-rebuild switch --flake /etc/nixos
#
# Review afterwards: git -C /etc/nixos diff   (commit it your way)
set -euo pipefail

TEMPLATE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/nixos-module.nix"
MODULE=/etc/nixos/openshell.nix
SYS_FLAKE=/etc/nixos/flake.nix
FOLDER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '\n== %s\n' "$*"; }
info() { printf '   %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- preflight
[[ $EUID -eq 0 ]] || die "run me with sudo"
[[ -r /etc/os-release ]] && grep -q '^ID=nixos$' /etc/os-release ||
  die "this script is for NixOS"
[[ -f "$TEMPLATE" ]] || die "template not found: $TEMPLATE"
[[ -f "$SYS_FLAKE" ]] || die "$SYS_FLAKE not found"
[[ -e "$MODULE" ]] || die "$MODULE not found — run install.sh first"
grep -q '^# managed-by: openshell-nixos' "$MODULE" ||
  die "$MODULE has no managed-by marker — it is hand-managed; edit it yourself"

TARGET_USER="${SUDO_USER:-leo}"
[[ "$TARGET_USER" != root ]] || TARGET_USER=leo
KSM_VAL="$(grep -o 'lib\.mkIf \(true\|false\)' "$MODULE" | head -1 | awk '{print $2}' || true)"
KSM_VAL="${KSM_VAL:-true}"
info "user: $TARGET_USER, ksm: $KSM_VAL, folder: $FOLDER"

# ------------------------------------------------------------------- relock
# The explicit path: prefix is load-bearing: with a bare directory nix
# auto-detects this folder's embedded .git and hashes it GIT-style (tracked
# files only), while builtins.getFlake "path:..." always uses the PATH
# fetcher (whole directory, .git included) — the two hashes differ and the
# rebuild dies with a NAR hash mismatch. Hash what getFlake will see.
log "relock: nix flake prefetch path:$FOLDER"
NARHASH="$(nix flake prefetch "path:$FOLDER" --json | sed -n 's/.*"hash":"\([^"]*\)".*/\1/p')"
[[ -n "$NARHASH" ]] || die "nix flake prefetch returned no hash"
REF="path:$FOLDER?narHash=$NARHASH"
info "ref: $REF"

# ------------------------------------------------------------------- render
log "render $TEMPLATE -> $MODULE"
SED_ARGS=(
  -e "s|@OPENSHELL_REF@|$REF|g"
  -e "s|@USER@|$TARGET_USER|g"
  -e "s|@KSM_ENABLED@|$KSM_VAL|g"
)
if ! getent group kvm >/dev/null; then
  info "no kvm group on this system — dropping the membership line"
  SED_ARGS+=( -e '/extraGroups = \[ "kvm" \]/d' )
fi

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
sed "${SED_ARGS[@]}" "$TEMPLATE" > "$TMP"

if cmp -s "$TMP" "$MODULE"; then
  info "unchanged (module already matches the template)"
else
  install -o root -g root -m 644 "$TMP" "$MODULE"
  info "written: $MODULE"
fi

# -------------------------------------------------------------------- track
log "track: git add openshell.nix"
if [[ ! -d /etc/nixos/.git ]]; then
  info "/etc/nixos is not a git worktree — visible to flake eval as-is"
elif git -C /etc/nixos ls-files --error-unmatch -- openshell.nix >/dev/null 2>&1; then
  git -C /etc/nixos add -- openshell.nix
  info "staged (flakes read tracked/staged files in a worktree)"
else
  git -C /etc/nixos add -- openshell.nix ||
    warn_hint="WARNING: git add failed — if the rebuild says openshell.nix is missing, run: git -C /etc/nixos add openshell.nix"
  info "${warn_hint:-added as new file}"
fi

# ------------------------------------------------------------------- switch
log "switch: nixos-rebuild switch --flake /etc/nixos"
nixos-rebuild switch --flake /etc/nixos

log "done"
echo "   Review: git -C /etc/nixos diff    (commit it your way)"
echo "   The flake ref now points at: $REF"
