# Design — guest-installed packages flow into a shared, guest-read-only cache

Companion to `dedup.md`. That document closes the **RAM** axis (Route A /
DAX). This designs the remaining **disk**-axis flow: a package installed
*inside a guest* must become instantly available to the next guest —
served from a local cache the guest cannot modify. Design only; nothing here is implemented.

**Status (2026-10-26) — podman companion now implements this flow.** The
`../openshell-podman/` stack (rootless containers, no KVM) runs the same
architecture end-to-end: the private guest store of §3 P exists as
`PRIVATE_STORE=1` in `run-sandbox`, with the consumer half realized as a
**symlink mirror + db seed** (host store directories symlinked into a
read-only `/opt/host-store`, top-level files copied because `--load-db`
opens them `O_NOFOLLOW`, host `--dump-db` loaded at boot) so host-known
paths are already-valid at boot and realize offline in ~94ms with zero
copies; the producer half is §3 P2's v1 inbox exactly (`nix copy --to
file://<workspace>/.nix-inbox` → `nix-ingest.path`/`.service` → import +
gcroot under `~/.local/share/openshell-gcroots`), riding the rw workspace
share so the cache port stays GET-only (§4). Measured: push auto-ingested
in ~4s, narinfo HTTP 200, fresh sandbox sees the path valid with zero
downloads (T-A/T-B in that README); ingest implements §4's first
mitigations — already-valid paths are never re-imported, and every
admitted path is archived with a host-*signed* narinfo receipt fetched
from `nix-serve` — leaving only the strict rebuild mode (mitigation 3)
open. The VM stack — the subject of this
document — remains design-only; the deltas, gotchas and measurements live
in `../openshell-podman/README.md` § Deduplication.

## 1. Goal, requirements, invariants

* **R1 — host → guests (already done):** anything on the host is at its
  exact content-addressed path in every guest, via the read-only `/nix`
  virtiofs mount ("host store IS guest store"). Zero bytes, zero copies.
* **R2 — guest → everyone:** a package first installed/built inside some
  guest must be one fast local retrieval away for the *next* guest that
  asks for it — no WAN, no rebuild.
* **R3 — read-only for guests:** guests can never modify the contents of
  the shared cache (their part of your original spec).
* **I1 — host-only-writes-`/nix`:** the shared store stays mounted `ro`
  forever. `SHARE_NIX_RW=1` remains a documented escape hatch, never the
  flow. (R3 is what makes I1 non-negotiable: a writable shared store is
  a cross-guest corruption/poisoning surface.)
* **I4 — mount budget:** x86_64 guests allow **3 bind mounts**
  (`VM_MAX_BIND_MOUNTS`, verified in `driver.rs`) — all three already
  used by the freebuff launcher. No fourth mount; the flow rides the
  network plus the existing `rw` `/workspace` share.

## 2. Prior art already on this machine (why this is a completion, not a greenfield)

Your note checks out — the flow was partially built on the
cloud-hypervisor stack with the SpectrumOS patches:

* **`../australVM/cloud_hypervisor_vm/configuration.nix`** — mounts the
  host `/nix` into the guest via virtiofs `ro` ("Mount Host Nix Store
  via DAX"), with a guest `nix-daemon` and SSH (the host↔VM data path
  for `nix copy`). `setup.sh` builds **cloud-hypervisor with the
  SpectrumOS `spectrum0` patches** (`spectrum-os.org …50.0-spectrum0…`);
  those add the shared-memory-region REST verbs
  (`GET_SHARED_MEMORY_REGIONS`/`SHMEM_MAP`/`SHMEM_UNMAP`) — their DAX
  equivalent, mirroring what Route A does for libkrun.
* **`../unfer/unfer_nixvm/README.md`** § *"A design decision this flake
  does not make unilaterally"* — states plainly that the **guest → host
  direction "does not yet work as configured"** (the guest's nix-daemon
  cannot write store paths back through the `ro` mount) and leaves two
  options open: **(a)** mount `/nix` rw and accept the trust boundary, or
  **(b)** *"run a second, guest-local Nix store and sync build results
  explicitly (e.g. `nix copy` over the existing SSH path)"* — *"safer,
  more code, not implemented here."*
* **`../spectrum/`** — SpectrumOS ships the consumer half as working
  practice: `release.nix` populates a binary cache, docs
  (`Documentation/doc/installation/binary-cache.adoc`) wire it as
  `nix.settings.substituters` — the "download very fast from a local
  cache" pattern.
* **This stack today** — inherits the host→guest half (credited in
  `README.md`), plus Route A for RAM. The producer half is the same gap
  unfer documented.

**This design is unfer's option (b), ported to OpenShell/libkrun** —
which has transports unfer's stack lacked (the `rw` `/workspace` share
and the gateway's guest↔host network) and doesn't need SSH.

## 3. Architecture

```
                    HOST (sole writer of /nix/store)
  ┌──────────────────────────────────────────────────────────┐
  │ /nix/store            authoritative, content-addressed   │
  │   ▲ ingest: verify + import + gcroot  (systemd path unit)│
  │ /workspace/.nix-inbox  ←── (2) producer: exported NARs    │
  │ nix-serve :5000        ──► (1) consumer: signed narinfo+nar│
  └───────────────▲──────────────────────▲───────────────────┘
                  │ (2) push             │ (1) substitute (HTTP, LAN)
        ┌─────────┴────────┐   ┌─────────┴─────────┐
        │ GUEST A          │   │ GUEST B           │
        │ private store    │   │ private store     │
        │ /var/lib/nix-…   │   │ (writable, local) │
        │ /nix  (ro mount) │◄──┤ same ro mount     │
        └──────────────────┘   └───────────────────┘
```

### P — private guest store (consumer side)
Each guest gets a **writable chroot store** on its own overlay
(e.g. `/var/lib/nix-private`, bounded by `overlay_disk_mib`), so
`nix profile install` has somewhere to write without ever touching
`/nix`. The guest's Nix settings are written by the driver as
`/etc/nix/nix.conf`, injected host-side into the overlay
(`inject_guest_nix_conf` → `write_rootfs_image_file` under `/upper/`,
the same channel as the boundary TLS material). **Not** via
`build_guest_environment` → `--vm-env`: that channel was tried and
cannot carry the value — libkrun encodes guest environment on the
kernel command line, where a multi-line value aborts the VMM build
with an `InvalidAscii` panic before the guest boots, and the guest's
`/etc` is read-only while init drop-ins run, so a boot-time script
cannot create the file either. nix reads `/etc/nix/nix.conf`
natively, so the delivery is equivalent:

```
substituters = http://10.0.128.1:5000   # host nix-serve, LAN only
trusted-public-keys = <host cache public key>
store = /var/lib/nix-private            # nothing lands in /nix
```

Resolution order gives your fast path for free: a package the host
already has is **in the ro `/nix` store itself** → referenced at zero
cost, no download; only *unknown* paths come from nix-serve (LAN) or
build locally.

### P2 — producer path (guest → shared)
After installing/building, the guest exports its closure as a binary
cache into the **existing** `rw` share:

```
nix copy --to file:///workspace/.nix-inbox <closure>   # .nar + .narinfo
```

A host **ingester** (systemd `.path` + `.service` watching the inbox)
runs `nix store import`, verifies path hashes, creates a gcroot under
`/nix/var/nix/gcroots/openshell-guest/`, then deletes the inbox entry.
From that moment **every future guest has the path instantly via the
existing ro mount** — a stronger result than a cache (no download at
all), and it also wins on the **RAM** axis: shared-store files are
served through virtiofs **with DAX**, whereas private-store copies sit
in each guest's own page cache — exactly the N+1 duplication
`dedup.md` measures.

No inbox-in-the-code-dir? **v2:** HTTP PUT ingest on the gateway
network (attic-style), same verify+import on the host; needs one
sandbox-policy allowlist entry (the same approval flow egress already
uses).

## 4. Security matrix (R3)

| surface | who can write | mechanism |
|---|---|---|
| `/nix/store` in guest | nobody (I1) | virtiofs `ro` |
| nix-serve cache | host only | HTTP GET for guests; no upload verb |
| push path | guests, *into inbox only* | inbox is not the cache; ingest is host-side |
| signing key | host only | nix rejects unsigned/wrong-key narinfos (`trusted-public-keys`) |
| served content | nix verifies | store-path hashes on import; a NAR cannot masquerade as a different path |

**Honest residual:** *input-addressed* store paths don't hash their
content, so hash-verification alone can't prove an imported IA path
holds what a nixpkgs derivation would have built. Mitigations, in
order of cost: (1) ingest re-exports **re-signed by the host** (guests
never trust guest-signed material); (2) ingest rejects paths already
present in `/nix/store`; (3) strict mode — host rebuilds the drv before
accepting (expensive; opt-in). Trust boundary note: guests are your own
agent workloads, and today's alternative (their private overlay) has no
stronger guarantees — but state it, don't hide it.

## 5. Quotas & lifecycle

* Private store bounded by `overlay_disk_mib` (4 GiB default) — document
  the limit; big builds need a bigger overlay, not a bigger shared store.
* Inbox capped (e.g. 2 GiB) with the ingester refusing beyond it.
* Ingested paths survive `nix-collect-garbage` via per-package gcroots;
  pruning = deleting gcroots (a deliberate host-side choice — the cache
  becomes "whatever the host chose to keep", which is what makes it safe).

## 6. Concrete deltas (smallest possible)

1. **nixos module:** `services.nix-serve.enable = true` (+ keypair),
   listen on the gateway-facing interface; `update-nixos.sh`/install
   re-render picks it up (module change → relock as usual).
2. **Fork driver (~10 lines):** gate a new opt-in flag
   (`guest_nix_cache`, default off, `--guest-nix-cache` /
   `OPENSHELL_VM_GUEST_NIX_CACHE`) that injects the settings as
   `/etc/nix/nix.conf` host-side into the overlay at launch
   (`inject_guest_nix_conf`, chained after the mount-manifest
   injection). Originally specified as `NIX_CONFIG` appended in
   `build_guest_environment`; end-to-end verification proved the
   `--vm-env` channel unable to carry a multi-line value (libkrun
   puts guest env on the kernel command line; the newline aborts the
   VMM build with an `InvalidAscii` panic at `builder.rs:594`), and a
   guest-init drop-in writing the file hits read-only `/etc` during
   init. Verified working in a live VM: the guest sees the settings
   at `/etc/nix/nix.conf`. *(Implemented.)*
3. **Guest dropin:** existing `GuestInitDropin` mechanism runs the
   `push` helper — no new plumbing. (Its `nix.conf` half moved to
   delta 2: drop-ins execute while `/etc` is still read-only, so the
   conf file is injected host-side into the overlay instead.)
4. **Host ingester:** two small systemd units + an `ingest.sh`.
5. **Policy:** allowlist entry for the nix-serve port (v1) or ingest
   port (v2).

## 7. Acceptance tests

* **T1 (consumer):** guest A `nix profile install nixpkgs#jq` for a
  path the host lacks → realized from `nix-serve` (LAN; packet/byte
  counter on the wire ≪ nixos.org) into the private store; runs.
* **T2 (producer roundtrip):** guest A pushes its closure; ingester
  imports; **fresh guest B** finds the path in `/nix` via the ro mount
  — zero network bytes, `ls` proves presence (the analogue of the
  fincore test for the RAM axis).
* **T3 (immutability, R3):** in-guest `echo x > /nix/store/<p>/file`
  → EROFS; guest PUT to nix-serve → rejected (no upload); tampered
  narinfo → nix refuses (signature).
* **T4 (N=2):** second guest installing the same package costs 0
  download for host-known paths and one LAN fetch for
  guest-produced-but-not-yet-ingested paths.

## 8. Explicitly rejected

* **(a) rw `/nix` mount** — the option unfer left open: it satisfies R2
  but destroys R3/I1 (any guest writes the store all guests read; a
  buggy or hostile guest becomes a supply-chain event). Keep as the
  documented `SHARE_NIX_RW` escape hatch only.
* Per-guest binary caches without ingest — they don't satisfy R2
  (nothing is shared), which is the whole point.
