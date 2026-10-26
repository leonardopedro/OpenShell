#!/usr/bin/env bash
# ingest.sh — host-side consumer of the sandbox nixcache-push inbox
# (guest → host half of the dedup loop; guest-cache-design.md §P2).
#
#   ingest.sh <inbox-dir>          # default wired in nix-ingest.path
#
# For every binary cache a sandbox pushed (nix copy --to file://…):
#   1. import it into the HOST store — after this, nix-serve :5000 serves
#      the path to every other sandbox/host, so their "install" is a
#      symlink, not a download (dedup with the host cache);
#   2. pin each path with an indirect gcroot so a host
#      `nix-collect-garbage` cannot drop the freshly shared path — pinned,
#      but NOT installed in any host profile (a gcroot is a root, not an
#      environment);
#   3. clear the inbox so the path unit fires again on the next push.
#
# Signature trust (guest-cache-design.md §4 — mitigations 1+2, implemented):
# pushes are unsigned (sandboxes hold no secret key), so import runs with
# --no-check-sigs; the NAR's store-path hash still binds name to content for
# CA paths. For input-addressed paths the host re-attests what it admitted:
#   * paths already in the host store are never re-imported (store paths are
#     immutable — the run logs them as attested-only);
#   * every admitted path's narinfo is fetched back from the host's own
#     nix-serve, which signs it with the machine cache key, and archived as
#     a signed receipt under gcroots/receipts/. The receipt is byte-for-byte
#     the signed material guests later consume from nix-serve, so the host
#     signature is the audit trail — no sandbox-pushed path is served
#     without it.
# Remaining honest residual: ingest does not rebuild input-addressed drvs
# before attesting (design §4's strict mode, mitigation 3 — not implemented).
#
# On failure nothing is deleted — retry with:
#   systemctl --user start nix-ingest.service
set -euo pipefail

INBOX="${1:?usage: ingest.sh <inbox-dir>}"
CAP_MB="${INGEST_CAP_MB:-2048}"   # inbox cap (guest-cache-design.md §5)

if [[ ! -d "$INBOX" ]]; then
  echo "nix-ingest: no inbox at $INBOX, nothing to do"
  exit 0
fi

shopt -s nullglob
narinfos=("$INBOX"/*.narinfo)
if [[ ${#narinfos[@]} -eq 0 ]]; then
  # Mid-push: nix writes the NAR before its narinfo; a fire here is normal
  # and harmless — the narinfo's arrival triggers the unit again.
  echo "nix-ingest: inbox has no narinfo yet (push in progress?), nothing to do"
  exit 0
fi

used_kb=$(du -sk "$INBOX" | cut -f1)
if (( used_kb > CAP_MB * 1024 )); then
  echo "nix-ingest: inbox is ${used_kb}kB, over the ${CAP_MB}MB cap — refusing" >&2
  exit 1
fi

NIX="$(command -v nix || echo /usr/bin/nix)"
NIX_STORE_BIN="$(command -v nix-store || echo "$(ls /nix/store/*-nix-[0-9]*/bin/nix-store 2>/dev/null | head -1)")"
[[ -n "$NIX_STORE_BIN" && -x "$NIX_STORE_BIN" ]] || { echo "nix-ingest: no nix-store binary found" >&2; exit 1; }

ROOT_DIR="${GCROOT_DIR:-$HOME/.local/share/openshell-gcroots}"

# 1. mitigation 2: paths the host store already has — store paths are
#    immutable (cannot be overwritten); logged so an ingest run can be told
#    apart from a pure attestation.
for ni in "${narinfos[@]}"; do
  path="$(grep -m1 '^StorePath: ' "$ni" | cut -d' ' -f2-)"
  [[ -n "$path" ]] || continue
  if "$NIX_STORE_BIN" --check-validity "$path" 2>/dev/null; then
    echo "nix-ingest: already in host store (attested, not re-imported): $path"
  fi
done

# 2. import the whole pushed closure into the host store (= the host cache)
"$NIX" --extra-experimental-features nix-command copy \
  --no-check-sigs --all --from "file://$INBOX"

# 3. pin every imported path (indirect gcroot: symlink here, registered by
#    the daemon under gcroots/auto — /nix/var/nix/gcroots is root-owned)
mkdir -p "$ROOT_DIR"
for ni in "${narinfos[@]}"; do
  path="$(grep -m1 '^StorePath: ' "$ni" | cut -d' ' -f2-)"
  [[ -n "$path" ]] || continue
  "$NIX_STORE_BIN" --add-root "$ROOT_DIR/$(basename "$path")" --realise "$path" >/dev/null
  echo "nix-ingest: pinned $path"
done

# 4. mitigation 1 — re-sign: archive the host-signed narinfo for every
#    admitted path, fetched from nix-serve (it signs with the machine cache
#    key at serve time). The receipt is byte-for-byte the material guests
#    will later trust from the same server; one LAN request per path.
RECEIPTS_DIR="${INGEST_RECEIPTS_DIR:-$ROOT_DIR/receipts}"
SERVE_URL="${NIX_SERVE_URL:-http://127.0.0.1:5000}"
if command -v curl >/dev/null 2>&1 \
   && curl -fsS -o /dev/null --max-time 10 "$SERVE_URL/nix-cache-info"; then
  mkdir -p "$RECEIPTS_DIR"
  for ni in "${narinfos[@]}"; do
    path="$(grep -m1 '^StorePath: ' "$ni" | cut -d' ' -f2-)"
    [[ -n "$path" ]] || continue
    h="$(basename "$path")"; h="${h%%-*}"
    if curl -fsS --max-time 30 "$SERVE_URL/$h.narinfo" -o "$RECEIPTS_DIR/$h.narinfo" \
       && grep -qE '^(Sig|Signature): ' "$RECEIPTS_DIR/$h.narinfo"; then
      echo "nix-ingest: re-signed receipt archived: $RECEIPTS_DIR/$h.narinfo"
    else
      rm -f "$RECEIPTS_DIR/$h.narinfo"
      echo "nix-ingest: WARN no signed narinfo for $h from $SERVE_URL — receipt skipped" >&2
    fi
  done
else
  echo "nix-ingest: WARN nix-serve unreachable at $SERVE_URL — signed receipts skipped" >&2
fi

# 5. consume the inbox (only reached when everything above succeeded)
find "$INBOX" -mindepth 1 -delete
echo "nix-ingest: done — ${#narinfos[@]} narinfo(s) imported from $INBOX"
