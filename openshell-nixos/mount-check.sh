#!/bin/sh
# mount-check.sh — verification payload for the OpenShell microVM. It runs
# INSIDE the guest (shared in through the rw ~/Projects mount as
# /workspace/openshell-nixos/mount-check.sh) and proves the three shared
# mounts that freebuff-sandbox sets up: /nix (ro), /var/lib/fnm (ro),
# /workspace (rw). Plain POSIX sh — the guest rootfs is a plain Ubuntu base.
# Ends with a sleep so the guest stays resident long enough to watch ksmd
# work against the booted guest's memory.

echo "== guest identity =="
id
uname -a

echo
echo "== mount table entries for the three shares =="
grep -E ' /nix | /var/lib/fnm | /workspace ' /proc/mounts || echo "(no entries — mounts FAILED)"

echo
echo "== 1. /nix — host Nix store, expected READ-ONLY =="
ls /nix/store | head -n 3
if [ -d /nix/store/hsn52cycm0ilav3jxnkpb7dx6msbgnw1-openshell-0.1.5 ]; then
  echo "OK: the host's own openshell store path is visible (content-addressed, zero copies)"
else
  echo "note: host openshell store path not seen"
fi
if touch /nix/store/.mount-check-write 2>/dev/null; then
  echo "FAIL: /nix accepted a write"
  rm -f /nix/store/.mount-check-write
else
  echo "OK: writes to /nix refused (ro)"
fi

echo
echo "== 2. /var/lib/fnm — the one node + freebuff install, expected READ-ONLY =="
ls /var/lib/fnm
NODE="$(find /var/lib/fnm -maxdepth 7 -name node 2>/dev/null | head -n 1)"
FREEBUFF="$(find /var/lib/fnm -maxdepth 7 -name freebuff 2>/dev/null | head -n 1)"
echo "node:     ${NODE:-NOT FOUND}"
echo "freebuff: ${FREEBUFF:-NOT FOUND}"
if [ -n "$NODE" ]; then "$NODE" --version; fi
if touch /var/lib/fnm/.mount-check-write 2>/dev/null; then
  echo "FAIL: /var/lib/fnm accepted a write"
  rm -f /var/lib/fnm/.mount-check-write
else
  echo "OK: writes to /var/lib/fnm refused (ro)"
fi

echo
echo "== 3. /workspace — host ~/Projects, expected READ-WRITE =="
ls /workspace | head -n 6
if echo "mount-check: rw ok" > /workspace/.mount-check-rw 2>/dev/null; then
  cat /workspace/.mount-check-rw
  rm -f /workspace/.mount-check-rw
  echo "OK: /workspace writable from inside the guest"
else
  echo "FAIL: /workspace not writable"
fi

echo
echo "== df in the guest =="
df -h / /nix /workspace 2>/dev/null || df -h

echo
echo "ALL MOUNT CHECKS DONE — keeping the guest resident 120s for ksmd"
sleep 120
