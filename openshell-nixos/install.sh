#!/usr/bin/env bash
# install.sh — install and configure NVIDIA OpenShell on this NixOS machine,
# with libkrun microVM <-> host sharing of the Nix store (disk), the shared
# node/freebuff toolchain (disk), and ksmd running for madvise-registered
# merging (RAM; nothing registers in this stack). Shared-file page-cache
# dedup comes from the fork driver's virtiofs DAX window (Route A — see
# README); ksmd stays as the secondary, madvise-based mechanism.
#
# Everything system-level is declared in /etc/nixos, as this machine's
# owner prefers: the OpenShell package, the KSM unit, the kvm group and
# loginctl linger all live in ONE module (openshell.nix) applied with
# `nixos-rebuild switch`. There is no `nix profile install`, and nothing
# is written into /etc/systemd — on NixOS that directory is a symlink into
# the immutable Nix store, so the imperative FHS recipe fails there by
# design (that is exactly how the first run of this installer died).
#
#   sudo ./install.sh [--force-config] [--no-ksm] [--no-wire] [--dry-run]
#                     [--user NAME]
#
# What it does, in order:
#   1. preflight   — root/NixOS/KVM/e2fsprogs checks
#   2. module      — render /etc/nixos/openshell.nix from this folder's
#                    nixos-module.nix template (locked getFlake ref to the
#                    package, @USER@, KSM on/off, kvm group). Rewritten
#                    only when the rendered result differs and the file
#                    still carries the managed-by marker
#   3. wire        — add './openshell.nix' to the system flake's modules
#                    list (idempotent, anchored on its own ./x.nix
#                    convention; --no-wire skips, for reruns after the
#                    first wiring)
#   4. switch      — `nixos-rebuild switch --flake /etc/nixos` (the package
#                    ref is content-locked inline via its narHash —
#                    flake.lock is not touched)
#   5. profile     — drop the leftover `nix profile` entry from the old
#                    recipe if one exists (the system profile now owns it)
#   6. gateway.toml— render ~/.config/openshell/gateway.toml (kept if
#                    present unless --force-config), then gate on the real
#                    `openshell-gateway config preflight` schema check
#   7. unit        — write ~/.config/systemd/user/openshell-gateway.service
#                    (adapted from the upstream unit shipped in the .deb)
#   8. start       — start the user manager, enable+start the gateway, wait
#                    until it answers on 127.0.0.1:17670
#   9. register    — `openshell gateway add ... --local` + select, as user
#
# Sudo is needed for: the /etc/nixos write, nixos-rebuild, the user unit
# write and starting the services. /etc/nixos is a git repo — review the
# changes with `git -C /etc/nixos diff` and commit them your way.
set -euo pipefail

# ----------------------------------------------------------------- options
FORCE_CONFIG=0
ENABLE_KSM=1
DRY_RUN=0
NO_WIRE=0
TARGET_USER="${SUDO_USER:-}"

usage() {
  sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//p;}' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --force-config) FORCE_CONFIG=1 ;;
    --no-ksm)       ENABLE_KSM=0 ;;
    --no-wire)      NO_WIRE=1 ;;
    --dry-run)      DRY_RUN=1 ;;
    --user)         TARGET_USER="${2:?--user needs a name}"; shift ;;
    -h|--help)      usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

log()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
info() { printf '   %s\n' "$*"; }
warn() { printf '   \033[33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '   \033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# run CMD... — executes, or only echoes under --dry-run.
run() {
  if [[ $DRY_RUN -eq 1 ]]; then
    printf '   \033[2m[dry-run] %s\033[0m\n' "$*"
  else
    "$@"
  fi
}

# user_bash CMD — run CMD in the target user's login shell (gets their PATH,
# home and NixOS profile wiring). REAL_* are set by preflight before use.
user_bash() {
  runuser -u "$REAL_USER" -- env HOME="$REAL_HOME" bash -lc "$1"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
MODULE=/etc/nixos/openshell.nix
SYS_FLAKE=/etc/nixos/flake.nix
SYS_LOCK=/etc/nixos/flake.lock

# --------------------------------------------------------------- preflight
log "preflight"

if [[ $DRY_RUN -eq 0 ]]; then
  [[ $EUID -eq 0 ]] || die "run me with sudo (or --dry-run to preview)"
  [[ -n "$TARGET_USER" && "$TARGET_USER" != root ]] ||
    die "run via sudo from your own account, or pass --user NAME"
fi
[[ -r /etc/os-release ]] && grep -q '^ID=nixos$' /etc/os-release ||
  die "this installer is for NixOS (found no ID=nixos)"
[[ -e "$SYS_FLAKE" ]] || die "$SYS_FLAKE not found — expected a flake-based NixOS install"

REAL_USER="${TARGET_USER:-${USER:-}}"
REAL_HOME="$(getent passwd "$REAL_USER" | cut -d: -f6)"
[[ -n "$REAL_HOME" ]] || die "cannot resolve home of '$REAL_USER'"
REAL_UID="$(id -u "$REAL_USER")"
REAL_GID="$(id -g "$REAL_USER")"
info "target user: $REAL_USER (uid $REAL_UID, home $REAL_HOME)"

command -v nix >/dev/null || die "nix not on PATH"
command -v nixos-rebuild >/dev/null || die "nixos-rebuild not on PATH"

if [[ -e /dev/kvm ]]; then
  info "KVM: /dev/kvm present ($(stat -c '%A %U:%G' /dev/kvm))"
else
  die "/dev/kvm missing — the MicroVM driver needs KVM (is virtualisation enabled in BIOS / are you bare-metal?)"
fi

HAS_KVM_GROUP=0
if getent group kvm >/dev/null; then
  HAS_KVM_GROUP=1
  if id -nG "$REAL_USER" | tr ' ' '\n' | grep -qx kvm; then
    info "kvm group: $REAL_USER already a member"
  else
    info "kvm group: $REAL_USER not a member yet — declared in the module, applied at the switch below"
  fi
else
  warn "no 'kvm' group on this system (device is $(stat -c '%A %U:%G' /dev/kvm)) — the module will omit the membership line"
fi

for tool in mke2fs debugfs e2fsck; do
  if command -v "$tool" >/dev/null; then
    info "e2fsprogs: $tool -> $(command -v "$tool")"
  else
    warn "$tool not on PATH — the gateway's own preflight re-checks this at every start"
  fi
done

# ------------------------------------------------------------- system module
log "system module: $MODULE"

# Locked reference to this folder as a flake: path + content hash. The
# module's builtins.getFlake() pulls exactly this build from the store.
if [[ $DRY_RUN -eq 1 ]]; then
  OS_REF="path:$SCRIPT_DIR?narHash=sha256-<nix-flake-prefetch>"
else
  # Explicit path: so the hash matches builtins.getFlake "path:..." below.
  # With a bare directory nix would auto-detect this folder's embedded .git
  # and hash GIT-style (tracked files only) while getFlake uses the PATH
  # fetcher (whole directory) — mismatched hashes fail the rebuild.
  NARHASH="$(nix flake prefetch "path:$SCRIPT_DIR" --json | sed -n 's/.*"hash":"\([^"]*\)".*/\1/p')"
  [[ -n "$NARHASH" ]] || die "nix flake prefetch $SCRIPT_DIR returned no hash"
  OS_REF="path:$SCRIPT_DIR?narHash=$NARHASH"
fi

if [[ $ENABLE_KSM -eq 1 ]]; then KSM_VAL=true; else KSM_VAL=false; fi

render_module() {  # template -> stdout, machine values substituted
  local sed_args=(
    -e "s|@OPENSHELL_REF@|$OS_REF|g"
    -e "s|@USER@|$REAL_USER|g"
    -e "s|@KSM_ENABLED@|$KSM_VAL|g"
  )
  if [[ $HAS_KVM_GROUP -eq 0 ]]; then
    sed_args+=( -e '/extraGroups = \[ "kvm" \]/d' )
  fi
  sed "${sed_args[@]}" "$SCRIPT_DIR/nixos-module.nix"
}

if [[ $DRY_RUN -eq 1 ]]; then
  run "render nixos-module.nix -> $MODULE (openshell ref, user=$REAL_USER, ksm=$KSM_VAL)"
elif [[ -e "$MODULE" ]] && ! grep -q '^# managed-by: openshell-nixos' "$MODULE"; then
  info "exists without the managed-by marker — hand-managed, leaving it untouched"
elif [[ -e "$MODULE" ]] && render_module | cmp -s - "$MODULE"; then
  info "unchanged (already matches the template)"
else
  render_module > "$MODULE.tmp"
  install -o root -g root -m 644 "$MODULE.tmp" "$MODULE"
  rm -f "$MODULE.tmp"
  info "written: $MODULE"
fi

# --------------------------------------------------------------- track module
# /etc/nixos is a git worktree and Nix evaluates flakes through git in a
# worktree — a file git does not know about is invisible to nixos-rebuild
# (this repo's own .gitignore header documents that rule).
if [[ $DRY_RUN -eq 1 ]]; then
  run "git -C /etc/nixos add openshell.nix   (only if untracked)"
elif [[ ! -e "$MODULE" ]]; then
  :
elif [[ ! -d /etc/nixos/.git ]]; then
  info "/etc/nixos is not a git worktree — the module is visible as-is"
elif git -C /etc/nixos ls-files --error-unmatch -- openshell.nix >/dev/null 2>&1; then
  info "git: openshell.nix already tracked"
else
  if git -C /etc/nixos add -- openshell.nix; then
    info "git: added openshell.nix (untracked files are invisible to flake eval)"
  else
    warn "could not 'git add' — if the rebuild reports openshell.nix missing, run: git -C /etc/nixos add openshell.nix"
  fi
fi

# ----------------------------------------------------------------- wire module
log "wire: ./openshell.nix into the modules list of $SYS_FLAKE"

if [[ $NO_WIRE -eq 1 ]]; then
  info "skipped (--no-wire)"
elif grep -qF './openshell.nix' "$SYS_FLAKE"; then
  info "already wired"
elif [[ $DRY_RUN -eq 1 ]]; then
  info "[dry-run] would insert './openshell.nix' after the last './x.nix' line of the modules list"
else
  LAST_LINE="$(grep -nE '^[[:space:]]*\./[^ ]*\.nix[[:space:]]*$' "$SYS_FLAKE" | tail -1 | cut -d: -f1 || true)"
  [[ -n "$LAST_LINE" ]] ||
    die "$SYS_FLAKE has no './x.nix' line to anchor on — add './openshell.nix' to its modules list by hand, then rerun"
  cp -a "$SYS_FLAKE" "$SYS_FLAKE.bak.openshell"
  # Append after the last ./x.nix line, reusing that line's indentation.
  sed -i "${LAST_LINE}s|^\([[:space:]]*\)\(\./[^ ]*\.nix[[:space:]]*\)$|\1\2\n\1./openshell.nix|" "$SYS_FLAKE"
  if ! grep -qF './openshell.nix' "$SYS_FLAKE"; then
    cp -a "$SYS_FLAKE.bak.openshell" "$SYS_FLAKE"
    die "anchor insert failed — nothing changed (backup: $SYS_FLAKE.bak.openshell)"
  fi
  info "wired after line $LAST_LINE (backup: $SYS_FLAKE.bak.openshell)"
fi

# --------------------------------------------------------------------- switch
log "switch: nixos-rebuild switch --flake /etc/nixos"

info "builds into one new system generation: openshell package, ksm-sharing unit,"
info "kvm group membership, linger — the getFlake ref is content-locked inline; flake.lock is untouched"
run nixos-rebuild switch --flake /etc/nixos ||
  die "nixos-rebuild failed — nothing after this point ran; fix the error above and rerun"

# ------------------------------------------------- drop imperative leftovers
log "clean up: remove the old 'nix profile install' entry (if any)"

if [[ $DRY_RUN -eq 1 ]]; then
  run "nix profile remove '$SCRIPT_DIR'#openshell   (as $REAL_USER, only if listed)"
elif user_bash "nix profile list 2>/dev/null" | grep -qF "$SCRIPT_DIR"; then
  info "found '$SCRIPT_DIR' in $REAL_USER's profile"
  # Remove by the entry's Name (as shown by `nix profile list`): a flake-ref
  # argument can silently match nothing while still exiting 0 — the first
  # run printed "removed" over nothing. Verify by re-listing instead of
  # trusting the exit code.
  if runuser -u "$REAL_USER" -- env HOME="$REAL_HOME" nix profile remove openshell; then
    if user_bash "nix profile list 2>/dev/null" | grep -qF "$SCRIPT_DIR"; then
      warn "entry still listed — remove it manually: nix profile remove openshell"
    else
      info "removed — the system profile provides openshell from now on"
    fi
  else
    warn "nix profile remove failed — run manually: nix profile remove openshell"
  fi
else
  info "nothing to remove"
fi

# ------------------------------------------------------------ resolve binaries
log "resolve the installed binaries (system profile)"

if [[ $DRY_RUN -eq 1 ]]; then
  GW_BIN="/run/current-system/sw/bin/openshell-gateway"
  CLI_BIN="/run/current-system/sw/bin/openshell"
  DRV_BIN="/run/current-system/sw/bin/openshell-driver-vm"
  info "after the switch: gateway/cli/driver from $(dirname "$GW_BIN")"
else
  # Prefer the system profile explicitly: a login shell's PATH puts
  # ~/.nix-profile ahead of /run/current-system/sw/bin, so a leftover
  # imperative entry would silently win — and pin the unit to a path the
  # cleanup step is about to delete (exactly what the first run did).
  SYS_BIN=/run/current-system/sw/bin
  if [[ -x "$SYS_BIN/openshell-gateway" ]]; then
    GW_BIN="$SYS_BIN/openshell-gateway"
    CLI_BIN="$SYS_BIN/openshell"
    DRV_BIN="$SYS_BIN/openshell-driver-vm"
    info "resolved from the system profile: $SYS_BIN"
  else
    GW_BIN="$(user_bash 'command -v openshell-gateway' || true)"
    CLI_BIN="$(user_bash 'command -v openshell' || true)"
    DRV_BIN="$(user_bash 'command -v openshell-driver-vm' || true)"
  fi
  [[ -x "${GW_BIN:-}" && -x "${CLI_BIN:-}" && -x "${DRV_BIN:-}" ]] ||
    die "openshell not found (checked $SYS_BIN and $REAL_USER's PATH) — check the rebuild output above, fix, and rerun"
  info "gateway: $GW_BIN"
  info "cli:     $CLI_BIN ($(user_bash 'openshell --version 2>/dev/null | head -1' || true))"
  info "driver:  $DRV_BIN (sibling of the gateway = its resolution path)"
fi

# ------------------------------------------------- fork driver (DAX build)
log "fork driver: DAX-enabled openshell-driver-vm (Route A)"

# The fork's driver passes --dax-window-mib through to libkrun's
# krun_add_virtiofs4 shm_size (the DAX window) and the guest init mounts
# those shares with -o dax. gateway.toml points driver_dir here so this
# binary wins over the packaged upstream driver (kept as fallback).
FORK_DRV="$SCRIPT_DIR/../target/release/openshell-driver-vm"
FORK_DRV_DIR="$REAL_HOME/.local/libexec/openshell"
FORK_DRV_DST="$FORK_DRV_DIR/openshell-driver-vm"
if [[ -x "$FORK_DRV" ]]; then
  if [[ $DRY_RUN -eq 1 ]]; then
    run "install -m 755 $FORK_DRV -> $FORK_DRV_DST   (gateway.toml driver_dir)"
  else
    mkdir -p "$FORK_DRV_DIR"
    install -o "$REAL_USER" -g "$REAL_GID" -m 755 "$FORK_DRV" "$FORK_DRV_DST"
    info "installed: $FORK_DRV_DST"
  fi
else
  warn "no fork build at $FORK_DRV — run:"
  warn "  OPENSHELL_VM_RUNTIME_COMPRESSED_DIR=target/vm-runtime-compressed cargo build --release -p openshell-driver-vm --features compute-driver"
  warn "gateway then falls back to the packaged driver (no DAX)"
fi

# --------------------------------------------------------------- gateway.toml
log "gateway config: $REAL_HOME/.config/openshell/gateway.toml"

CFG_DIR="$REAL_HOME/.config/openshell"
CFG="$CFG_DIR/gateway.toml"

if [[ -e "$CFG" && $FORCE_CONFIG -eq 0 ]]; then
  info "exists — keeping it (rerun with --force-config to overwrite from the template)"
else
  run mkdir -p "$CFG_DIR"
  if [[ $DRY_RUN -eq 1 ]]; then
    run "render gateway.toml (template: $SCRIPT_DIR/gateway.toml, @HOME@ -> $REAL_HOME, @SANDBOX_UID@/@SANDBOX_GID@ -> $REAL_UID:$REAL_GID) > $CFG"
  else
    sed -e "s|@HOME@|$REAL_HOME|g" \
        -e "s|@SANDBOX_UID@|$REAL_UID|g" \
        -e "s|@SANDBOX_GID@|$REAL_GID|g" \
        "$SCRIPT_DIR/gateway.toml" > "$CFG.tmp"
    install -o "$REAL_USER" -g "$REAL_GID" -m 644 "$CFG.tmp" "$CFG"
    rm -f "$CFG.tmp"
    info "rendered from template"
  fi
fi

if [[ $DRY_RUN -eq 0 ]]; then
  info "schema gate: openshell-gateway config preflight"
  if ! user_bash "OPENSHELL_GATEWAY_CONFIG='$CFG' '$GW_BIN' config preflight"; then
    die "preflight rejected $CFG — fix it (or rerun with --force-config to reset to the template) before starting anything"
  fi
  info "preflight: OK"
fi

# --------------------------------------------------------------- gateway.env
# The unit loads EnvironmentFile=-%E/openshell/gateway.env; systemd starts
# the gateway with that environment and the gateway passes it on to the
# driver it spawns. The driver reads OPENSHELL_VM_DAX_WINDOW_MIB (clap env)
# and hands every virtiofs share a DAX window of that size, so shared files
# live once in the host page cache instead of once per guest.
# gateway.toml itself cannot carry the key: the packaged gateway parses
# [openshell.drivers.vm] with deny_unknown_fields.
log "gateway env: $REAL_HOME/.config/openshell/gateway.env"

DAX_WINDOW_MIB=128
GATEWAY_ENV="$CFG_DIR/gateway.env"
ENV_CHANGED=0
if [[ $DRY_RUN -eq 1 ]]; then
  run "ensure OPENSHELL_VM_DAX_WINDOW_MIB=$DAX_WINDOW_MIB in $GATEWAY_ENV"
elif [[ -f "$GATEWAY_ENV" ]] && grep -q '^OPENSHELL_VM_DAX_WINDOW_MIB=' "$GATEWAY_ENV"; then
  info "present: $(grep '^OPENSHELL_VM_DAX_WINDOW_MIB=' "$GATEWAY_ENV")"
else
  printf 'OPENSHELL_VM_DAX_WINDOW_MIB=%s\n' "$DAX_WINDOW_MIB" >> "$GATEWAY_ENV"
  chown "$REAL_USER:$REAL_GID" "$GATEWAY_ENV"
  chmod 640 "$GATEWAY_ENV"
  ENV_CHANGED=1
  info "appended OPENSHELL_VM_DAX_WINDOW_MIB=$DAX_WINDOW_MIB (edit gateway.env to resize or disable)"
fi

# ------------------------------------------------------------- user service
log "gateway service: ~/.config/systemd/user/openshell-gateway.service"

UNIT_DIR="$REAL_HOME/.config/systemd/user"
UNIT="$UNIT_DIR/openshell-gateway.service"

run mkdir -p "$UNIT_DIR"
if [[ $DRY_RUN -eq 1 ]]; then
  run "write unit (ExecStart=$GW_BIN) > $UNIT"
else
  cat > "$UNIT.tmp" <<EOF
# Adapted from the upstream unit shipped in OpenShell's .deb (Apache-2.0),
# with Nix store paths made explicit and a deterministic PATH so the VM
# driver finds e2fsprogs (mke2fs/debugfs) instead of whatever root saw.
[Unit]
Description=OpenShell Gateway (libkrun MicroVM driver)
Documentation=https://github.com/NVIDIA/OpenShell
After=default.target

[Service]
Type=simple
StateDirectory=openshell/gateway
Environment=OPENSHELL_LOCAL_TLS_DIR=%h/.local/state/openshell/tls
Environment=PATH=/run/current-system/sw/bin:%h/.nix-profile/bin
EnvironmentFile=-%E/openshell/gateway.env
ExecStartPre=$GW_BIN config preflight
ExecStartPre=$GW_BIN generate-certs --output-dir %h/.local/state/openshell/tls --server-san host.openshell.internal
ExecStart=$GW_BIN
Restart=on-failure
RestartSec=5s
PrivateTmp=true
UMask=0077

[Install]
WantedBy=default.target
EOF
  UNIT_CHANGED=1
  if [[ -f "$UNIT" ]] && cmp -s "$UNIT.tmp" "$UNIT"; then
    UNIT_CHANGED=0
    rm -f "$UNIT.tmp"
    info "unit unchanged (ExecStart=$GW_BIN)"
  else
    install -o "$REAL_USER" -g "$REAL_GID" -m 644 "$UNIT.tmp" "$UNIT"
    rm -f "$UNIT.tmp"
    info "unit written (ExecStart=$GW_BIN)"
  fi
fi

# ------------------------------------------------------------ start & wait
log "start: user manager + gateway"

# linger itself now comes declaratively from openshell.nix; we only need the
# manager running right now (no reboot required).
run systemctl start "user@$REAL_UID.service"
if [[ $DRY_RUN -eq 1 ]]; then
  printf '   \033[2m[dry-run] systemctl --user daemon-reload, enable, (re)start openshell-gateway (as %s)\033[0m\n' "$REAL_USER"
else
  runuser -u "$REAL_USER" -- env XDG_RUNTIME_DIR="/run/user/$REAL_UID" \
    systemctl --user daemon-reload
  user_sysctl() {
    runuser -u "$REAL_USER" -- env XDG_RUNTIME_DIR="/run/user/$REAL_UID" systemctl --user "$@"
  }
  user_sysctl enable openshell-gateway
  # (Re)start only when the unit content or gateway.env actually changed or
  # the gateway is down — bouncing a healthy gateway on every rerun would
  # drop sessions.
  if [[ ${UNIT_CHANGED:-0} -eq 1 ]] || [[ ${ENV_CHANGED:-0} -eq 1 ]] || ! user_sysctl is-active --quiet openshell-gateway; then
    info "starting openshell-gateway (unit or env changed, or not running)"
    user_sysctl restart openshell-gateway
  else
    info "openshell-gateway already running with this unit"
  fi

  info "waiting for the gateway to answer on https://127.0.0.1:17670 ..."
  # curl if root's PATH has it; otherwise a plain TCP connect (TLS not needed
  # just to see the port come up). Deliberately NOT -f: the gateway answers
  # GET / with 404 (it speaks gRPC), so an HTTP status — any HTTP status —
  # means "listening"; only a failed transfer means "down". With -f the
  # first run reported a perfectly healthy gateway as down for 30s (the
  # journal shows these exact probes being served 404).
  probe() {
    if command -v curl >/dev/null; then
      curl -ks --max-time 2 -o /dev/null https://127.0.0.1:17670/ 2>/dev/null
    else
      timeout 2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/17670' 2>/dev/null
    fi
  }
  UP=0
  for _ in $(seq 1 30); do
    if probe; then
      UP=1; break
    fi
    sleep 1
  done
  if [[ $UP -eq 1 ]]; then
    info "gateway is up"
  else
    warn "gateway not answering after 30s — check:"
    warn "  journalctl --user -u openshell-gateway -n 50 --no-pager"
    warn "  $GW_BIN config preflight"
  fi
fi

# --------------------------------------------------------------- register CLI
log "register the gateway with the CLI (as $REAL_USER)"

if [[ $DRY_RUN -eq 1 ]]; then
  run "openshell gateway add https://127.0.0.1:17670 --local --name local"
  run "openshell gateway select local"
else
  if user_bash "'$CLI_BIN' gateway add https://127.0.0.1:17670 --local --name local"; then
    info "registered as 'local'"
  else
    warn "gateway add failed (already registered, or gateway not up yet) — try: openshell gateway list"
  fi
  user_bash "'$CLI_BIN' gateway select local" >/dev/null 2>&1 ||
    warn "could not select 'local' — try: openshell gateway select local"
  user_bash "'$CLI_BIN' status" || true
fi

# ------------------------------------------------------------------- done
log "done"
cat <<EOF
   OpenShell: libkrun microVM driver, gateway on https://127.0.0.1:17670
   Declared in /etc/nixos (review: git -C /etc/nixos diff):
     openshell.nix — package + ksm-sharing.service + kvm group + linger
     flake.nix     — modules list gains ./openshell.nix (idempotent)
   Sharing enabled:
     disk  — /nix           (host store IS guest store, read-only)
             /var/lib/fnm    (one copy of node + freebuff/opencode/repomix)
     ram   — virtiofs DAX window on every share (fork driver, Route A):
             guest maps host page-cache frames directly, no per-guest
             page-cache copies (OPENSHELL_VM_DAX_WINDOW_MIB, gateway.env);
             ksmd active in madvise mode (ksm-sharing.service)
   Next:
     1. re-login once so \$USER's kvm group membership settles — or just start a sandbox now
     2. $SCRIPT_DIR/freebuff-sandbox              # freebuff in a microVM, 3 shared mounts
     3. journalctl --user -u openshell-gateway -f  # gateway logs
   Security decisions are documented in $SCRIPT_DIR/README.md
   (read-only /nix, libkrun mount caveats, the rw workspace boundary).
EOF
