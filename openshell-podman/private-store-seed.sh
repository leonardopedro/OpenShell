#!/bin/sh
# private-store-seed.sh — runs INSIDE the sandbox, before the workload.
# run-sandbox wraps the command with it when PRIVATE_STORE=1:
#
#   sh -c '/opt/seed/seed.sh && exec "$@"' seed-guard <workload...>
#
# What it does: the staged private store (/nix/store, a host dir bind) is
# pre-populated HOST-side with a mirror of the host store — directories as
# symlinks into the read-only /opt/host-store mirror (zero bytes), top-level
# regular files as copies (nix's load-db opens them O_NOFOLLOW; measured
# ~27MB total). This script completes the dedup by loading the host's db
# dump, so nix inside the sandbox considers those paths *installed*:
#
#   realizing a host-known path  -> zero downloads, zero new content
#   (it is a mirrored symlink + a ValidPaths row, not a copy)
#
# Anything the host does NOT have is fetched from the host cache
# (host.openshell.internal:5000) or built — and lands as a real file in this
# sandbox's private store. Push it back with nixcache-push to make it a
# host-cache path for everyone (guest-cache-design.md §P2).
#
# The dump is produced by the SAME nix-store binary this script loads it
# with (path written to /opt/seed/nix-store-path by run-sandbox), so dump and
# load formats always match.
set -eu

DB=/nix/var/nix/db/db.sqlite
# Completeness marker, written only after a successful --load-db. Phase
# `Ready` precedes seed completion (measured ~6–20s: load-db checks every
# derivation), so a create killed mid-seed leaves an empty/partial db.sqlite
# behind — and a plain `-e` guard would then seal that partial db forever
# (measured: every later check-validity fails, "not valid"). Reload instead.
if [ -e "$DB" ]; then
  if [ -e "$DB.seed-ok" ]; then
    echo "seed: host db already present, skipping"
    exit 0
  fi
  echo "seed: incomplete db from an interrupted seed — reloading" >&2
  rm -f "$DB" "$DB-shm" "$DB-wal"
fi

# State dir must exist before nix first runs, or nix assumes it runs in a
# chroot and relocates its db (measured: reads then fail with ENOENT paths
# under the relocated root).
mkdir -p /nix/var/nix

NIX_STORE=$(cat /opt/seed/nix-store-path)
[ -x "$NIX_STORE" ] || { echo "seed: $NIX_STORE not executable" >&2; exit 1; }

echo "seed: loading host db dump ($(wc -c < /opt/seed/db.dump) bytes)..."
"$NIX_STORE" --load-db < /opt/seed/db.dump
: > "$DB.seed-ok"

echo "seed: done — $(ls /nix/store | wc -l) store entries, $(find /nix/store -maxdepth 1 -type f | wc -l) real files (rest are symlinks into /opt/host-store)"
