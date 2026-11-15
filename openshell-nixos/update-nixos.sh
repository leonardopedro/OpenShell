#!/usr/bin/env bash
# update-nixos.sh — re-render /etc/nixos/openshell.nix from this folder's
# nixos-module.nix template and apply it. Run as sudo:
#
#   sudo ./update-nixos.sh
#
# Why this exists: keep /etc/nixos in sync with this folder's template and
# package derivation. There is no flake ref to relock anymore: the module
# callPackages ./openshell-package.nix, a plain file this script copies
# alongside it. (The old mechanism pinned this folder as
# builtins.getFlake "path:<folder>?narHash=..." — when the folder changed,
# the pin went stale and the NEXT nixos-rebuild died at eval with a NAR
# hash mismatch; this script's whole "relock" step existed to chase that.
# Both are gone.)
#
# What it does:
#   1. preflight  — root, template + package file exist, module still
#                   carries the managed-by marker (hand-managed files are
#                   left alone)
#   2. package    — copy openshell-package.nix to /etc/nixos (idempotent)
#   3. render     — @USER@ (sudo caller), @KSM_ENABLED@ (kept as found in
#                   the current module, default true), kvm group line kept
#                   iff the group exists
#   4. track      — git add openshell.nix + openshell-package.nix (files a
#                   git worktree does not track are invisible to flake eval)
#   5. switch     — nixos-rebuild switch --flake /etc/nixos
#
# Review afterwards: git -C /etc/nixos diff   (commit it your way)
set -euo pipefail

FOLDER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$FOLDER/nixos-module.nix"
PKG_SRC="$FOLDER/openshell-package.nix"
MODULE=/etc/nixos/openshell.nix
PKG_DEST=/etc/nixos/openshell-package.nix
SYS_FLAKE=/etc/nixos/flake.nix

log() { printf '\n== %s\n' "$*"; }
info() { printf '   %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- preflight
[[ $EUID -eq 0 ]] || die "run me with sudo"
[[ -r /etc/os-release ]] && grep -q '^ID=nixos$' /etc/os-release ||
  die "this script is for NixOS"
[[ -f "$TEMPLATE" ]] || die "template not found: $TEMPLATE"
[[ -f "$PKG_SRC" ]] || die "package file not found: $PKG_SRC"
[[ -f "$SYS_FLAKE" ]] || die "$SYS_FLAKE not found"
[[ -e "$MODULE" ]] || die "$MODULE not found — run install.sh first"
grep -q '^# managed-by: openshell-nixos' "$MODULE" ||
  die "$MODULE has no managed-by marker — it is hand-managed; edit it yourself"

TARGET_USER="${SUDO_USER:-leo}"
[[ "$TARGET_USER" != root ]] || TARGET_USER=leo
KSM_VAL="$(grep -o 'lib\.mkIf \(true\|false\)' "$MODULE" | head -1 | awk '{print $2}' || true)"
KSM_VAL="${KSM_VAL:-true}"
info "user: $TARGET_USER, ksm: $KSM_VAL, folder: $FOLDER"

# ------------------------------------------------------------------ package
log "package: $PKG_SRC -> $PKG_DEST"
if [[ -e "$PKG_DEST" ]] && cmp -s "$PKG_SRC" "$PKG_DEST"; then
  info "unchanged"
else
  install -o root -g root -m 644 "$PKG_SRC" "$PKG_DEST"
  info "written: $PKG_DEST"
fi

# ------------------------------------------------------------------- render
log "render $TEMPLATE -> $MODULE"
SED_ARGS=(
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
log "track: git add openshell.nix openshell-package.nix"
if [[ ! -d /etc/nixos/.git ]]; then
  info "/etc/nixos is not a git worktree — visible to flake eval as-is"
else
  for f in openshell.nix openshell-package.nix; do
    if git -C /etc/nixos ls-files --error-unmatch -- "$f" >/dev/null 2>&1; then
      info "$f: already tracked (staged any change above)"
    elif git -C /etc/nixos add -- "$f"; then
      info "$f: added (flakes read tracked/staged files in a worktree)"
    else
      info "WARNING: git add $f failed — if the rebuild says it is missing, run: git -C /etc/nixos add $f"
    fi
  done
fi

# ------------------------------------------------------------------- switch
log "switch: nixos-rebuild switch --flake /etc/nixos"
nixos-rebuild switch --flake /etc/nixos

log "done"
echo "   Review: git -C /etc/nixos diff    (commit it your way)"
echo "   The package builds from: $PKG_DEST (no flake ref, nothing to relock)"
