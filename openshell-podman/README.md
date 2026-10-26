# openshell-podman — OpenShell on a machine without KVM, with the host Nix store shared into every sandbox

Companion to [`../openshell-nixos/`](../openshell-nixos/), which runs the
libkrun MicroVM variant on a KVM-capable NixOS host. This folder runs the
same *sharing design* on a machine where VMs are not an option — Oracle
Linux 10 on aarch64, no `/dev/kvm` — using OpenShell's **rootless Podman
compute driver** instead.

```text
host (rootless, no daemon beyond podman.socket)
┌──────────────────────────────────────┐      ┌────────────────────────────────┐
│ openshell-gateway (user svc)         │      │ sandbox container (per sandbox)│
│  └ compute_driver = "podman"         │──────▶  workload: network=none,       │
│ nix-serve :5000 (user svc, signed)   │ bind │  Landlock + seccomp           │
│ /nix/store  ← one copy (disk)        │      │  /nix/store+var (rw, seeded)  │
│ host page cache serves every read    │◀────▶│  /etc/nix/nix.conf (ro, cache) │
│  — binds share it by construction    │ pages│  /var/lib/fnm   (ro, = host)   │
└──────────────────────────────────────┘      │  /workspace     (rw, ~/Proj…) │
        ▲ policy: nix_cache endpoint :5000    └────────────────────────────────┘
        └──────────── nix (from the shared store) substitutes via supervisor
```

**State of this folder (2026-10-26):** the loop is implemented end to end
and measured. `PRIVATE_STORE=1` is the default (a private dedup store,
staged per create), `ingest.sh` re-attests everything a sandbox pushes
(host-signed narinfo receipts), and `./acceptance-dedup` — the design
doc's T-A/T-B pair as one rerunnable check — passes **25/25**. Run it
after any change to this folder. The one piece of `guest-cache-design.md`
§4 still open is the strict mode: ingest does not yet rebuild
input-addressed drvs before attesting them.

## Why podman, here

The [support matrix](https://docs.nvidia.com/openshell/latest/about/support-matrix)
options for Linux are Docker, Podman, Kubernetes and MicroVM. On this
machine: MicroVM needs KVM (absent — it could never boot), Kubernetes needs
a cluster (absurd for one user on one box), Docker wants a root daemon for
a gateway that runs as an ordinary user. Rootless Podman is the documented
pick for "rootless Linux workstations without a Docker daemon": no daemon,
one user socket, cgroups v2 (present), and the same bind-mount vocabulary
the VM driver uses.

## The sharing, mapped from VMs to containers

The VM stack engineered its sharing; the container stack inherits it:

| Axis | `openshell-nixos` (microVM) | this folder (podman) |
|---|---|---|
| disk: one copy of `/nix` | virtiofs share of the host store, `ro` | bind mount of the host store, `ro` |
| RAM: one copy of shared files | DAX window — guest maps host page-cache frames (Route A) | **free**: container reads *are* host page-cache reads; no window to arrange |
| KSM (`ksm-sharing.service`) | documented as unable to contribute (file-backed pages) | not needed for the same reason, and not present |
| nixcache, host → sandbox | `guest_nix_cache` injects `/etc/nix/nix.conf` into the guest overlay | `nix.conf` bound in at `/etc/nix/nix.conf` (same fields: substituters, trusted keys, `connect-timeout`, `fallback`) |
| nixcache endpoint | host `nix-serve :5000` (LAN), design doc steps 1–3 | identical: `nix-serve.service` here, one policy rule in `sandbox-policy.yaml` |
| workload identity | `sandbox_uid/gid` pinned in gateway.toml | nothing to pin: rootless podman runs as the gateway user (uid 1000 = the image default) |
| mounts per sandbox | 3 (x86_64 `VM_MAX_BIND_MOUNTS`) | no cap; `run-sandbox` uses 3–4 |

What does **not** carry over: `driver_dir`/DAX knobs, `state_dir`,
e2fsprogs tooling, the KSM unit — all VM-driver specifics. The trust
posture does carry over: bind mounts are opt-in three ways
(`allow_driver_config`, `enable_bind_mounts`, resource admission off),
`/nix` stays read-only, and file access inside the workload is still
gated by Landlock rules derived from the policy.

## Install (two sudo steps, then everything is user-level)

```sh
# 1. system: podman + a subuid/subgid range for the gateway user
#    (rootless podman maps container uids through it; the default user
#     in /etc/subuid does not cover this account)
sudo dnf install -y podman
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER

# 2. user socket (rootless driver transport)
systemctl --user enable --now podman.socket

# 3. build this tree (see the unit template for why prebuilt-z3)
cargo build --release -p openshell-cli -p openshell-gateway \
    -p openshell-prover --features openshell-prover/prebuilt-z3
install -Dm755 target/release/openshell-gateway ~/.local/bin/openshell-gateway
install -Dm755 target/release/openshell         ~/.local/bin/openshell

# 4. the nixcache: signing key, host cache service
mkdir -p ~/.config/nix-serve
nix key generate-secret --key-name cache-1 > ~/.config/nix-serve/cache-1.key
nix key convert-secret-to-public < ~/.config/nix-serve/cache-1.key \
    > ~/.config/nix-serve/cache-1.pub
chmod 600 ~/.config/nix-serve/cache-1.key
install -Dm644 nix-serve.service ~/.config/systemd/user/nix-serve.service
nix profile install nixpkgs#nix-serve
systemctl --user daemon-reload && systemctl --user enable --now nix-serve
curl -s http://127.0.0.1:5000/nix-cache-info   # StoreDir: /nix/store

# 5. render the machine-specific files. Only the cache public key varies by
#    machine: nix.conf and sandbox-policy.yaml both reach the cache through
#    host.openshell.internal (OpenShell's alias for the gateway host), so no
#    LAN address needs substituting anywhere — see nix.conf's header for why
#    a LAN name or IP cannot work from inside a podman sandbox.
mkdir -p ~/.config/openshell/nix-conf
sed -e "s|@CACHE_PUBKEY@|$(cat ~/.config/nix-serve/cache-1.pub)" \
    nix.conf > ~/.config/openshell/nix-conf/nix.conf
install -Dm644 sandbox-policy.yaml ~/.config/openshell/sandbox-policy.yaml
install -Dm644 gateway.toml ~/.config/openshell/gateway.toml
OPENSHELL_GATEWAY_CONFIG=$HOME/.config/openshell/gateway.toml \
    ~/.local/bin/openshell-gateway config preflight   # must exit 0

# 6. gateway service + registration
install -Dm644 openshell-gateway.service \
    ~/.config/systemd/user/openshell-gateway.service
systemctl --user daemon-reload && systemctl --user enable --now openshell-gateway
openshell gateway add https://127.0.0.1:17670 --local --name local
openshell gateway select local
openshell status

# 7. the dedup loop (guest -> host cache): workspace write access for the
#    workload — its uid maps to host 100999 while the workspace is owned by
#    your uid, so without the ACL every workload write to /workspace is
#    EACCES (measured; this gap predates the dedup work) — plus the inbox
#    ingester. PRIVATE_STORE=1 is now the default, so this block is part of
#    setup unless you always pass PRIVATE_STORE=0. NB the unit runs the
#    INSTALLED ingest.sh: reinstall this line after every edit (a stale
#    copy silently ran the old script — measured, receipts never written).
setfacl -m u:100999:rwx "$HOME/Projects"
setfacl -m d:u:1000:rwx,d:u:100999:rwx,d:mask::rwx "$HOME/Projects"
install -Dm755 ingest.sh ~/.local/libexec/openshell/ingest.sh
install -Dm644 nix-ingest.service nix-ingest.path ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now nix-ingest.path
# PROJECTS_DIR other than ~/Projects? re-render the %h/Projects line in
# nix-ingest.path first (it is the watched inbox's parent).
```

## Run a sandbox

```sh
./run-sandbox                          # PRIVATE_STORE on (default): dedup mirror, parked sandbox
./run-sandbox -- node --version        # node comes from /var/lib/fnm, not the image
SANDBOX_NAME=fb2 PROJECTS_DIR=~/code ./run-sandbox -- freebuff "fix the tests"
PRIVATE_STORE=0 ./run-sandbox          # old shape: shared read-only /nix, no staging
DRY_RUN=1 ./run-sandbox                # print the openshell command instead
```

Unless `PRIVATE_STORE=0` is set, the sandbox gets the private dedup store
described below (staged on every create, ~10–20s); `PRIVATE_STORE=0`
restores the pre-2026-10-26 shared read-only shape exactly — see
"Migration" at the end of that section.

Whatever command you pass is guarded: it only starts after the seed
finishes. If you drive the sandbox with `openshell sandbox exec` right
after create instead, wait for `seed: done` in the create output first —
phase `Ready` arrives ~6–20s earlier, while the host db dump is still
loading, and probing validity in that window returns arbitrary "not
valid" results (measured; see the timing note in the dedup section).

`run-sandbox` passes `--driver-config-json '{"podman":{"mounts":[…]}}'`
(the exact shape `../openshell-nixos/freebuff-sandbox` uses for `vm`) and
`--policy ~/.config/openshell/sandbox-policy.yaml`.

## Deduplication: `PRIVATE_STORE=1` and the guest → host loop

The shared store dedups by construction, but it is read-only — a sandbox
that must *write* (profiles, builds, substituting a path the host lacks)
needs its own store, and a naive private store re-downloads every byte the
host already has. `PRIVATE_STORE=1` gives a private store that still
dedups against the host cache:

```sh
PRIVATE_STORE=1 SANDBOX_NAME=mybuild ./run-sandbox   # staged fresh per run
```

**Consumer side (host → sandbox).** Staging runs host-side, takes seconds:

* every host store *directory* becomes a **symlink** into the read-only
  `/opt/host-store` mirror — the bulk of the store, zero bytes copied;
* top-level *regular files* (drvs + file outputs) are **copied** — measured
  25MB + 2.2MB, and required: `nix-store --load-db` opens them with
  `O_NOFOLLOW` (a symlink fails with `ELOOP`, measured) and refuses rows
  whose file is absent (also measured), while directory entries are only
  lstat'd and symlink fine;
* the host db dump (`nix-store --dump-db`) is loaded at boot by
  `private-store-seed.sh`, so every host path is already *valid* in the
  sandbox.

**Timing (measured 2026-10-26):** phase `Ready` precedes `seed: done` by
~6–20s (`--load-db` checks every derivation before returning). The workload
itself is guarded — it only starts via `seed && exec` — but commands run
through `openshell sandbox exec` in that window race a partially-loaded db
and see arbitrary "not valid" results. Scripts should wait for
`seed: done` in the create output before probing validity
(`acceptance-dedup` does). If a create is killed mid-seed, the partial db
is detected on the next run by the missing `.seed-ok` marker and reloaded.

Measured 2026-10-26: the sandbox store is **29MB** against the host store's
**2.5G**, and `nix-store -r <drv> --option substituters ''` returns rc=0 in
~94ms with **zero** new store entries, zero new files and zero network —
realizing a host package in the private store is a symlink plus a db row,
not an install. Paths the host lacks are fetched from
`host.openshell.internal:5000` or built, and land as real files in the
private store.

State dirs are fresh `mktemp` dirs per run: container-written files belong
to the rootless mapping (host uid 100999) and cannot be `rm`'d by you — an
in-place wipe once failed silently and poisoned the db (validity rows
without files → every build died with `opening file '/nix/store/…': No
such file or directory`). After deleting the sandbox:

```sh
podman unshare rm -rf /tmp/openshell-store.*
```

**Producer side (sandbox → host cache).** `nix-serve` has no upload, so
pushes ride the rw workspace share — no extra policy rule, no open port:

```sh
# inside the sandbox (the helper lives in the workspace when the repo does):
./nixcache-push /nix/store/…-mypkg          # nar + narinfo into .nix-inbox
```

`nix-ingest.path` (install step 7) fires `nix-ingest.service`, whose
`ingest.sh` imports the closure into the **host store** — from that moment
`nix-serve` serves it to every sandbox and host — pins each path with a
gcroot (protected from GC, but *not* installed in any host profile), and
clears the inbox. Two ingest-time attestations cover design §4's
input-addressed gap (mitigations 1+2): paths already in the host store are
never re-imported (store paths are immutable; logged as attested-only), and
every admitted path's narinfo is fetched back from the host's own
`nix-serve` — which signs it with the machine cache key — and archived as a
**signed receipt** under `~/.local/share/openshell-gcroots/receipts/`. The
receipt is byte-for-byte the signed material guests later consume, so the
host's signature is the audit trail for what a sandbox pushed; only the
strict rebuild mode (mitigation 3) stays unimplemented. Measured: push → ingested automatically in **~4s**,
narinfo HTTP 200, and a *fresh* `PRIVATE_STORE` sandbox created afterwards
finds the path valid at seed time with `~/.cache/nix` absent — zero
downloads. That is the full loop: the first install lands in the host
(cache) store, every later sandbox "installs" a symlink.

Trust split (design §4): the inbox is not the cache — sandboxes write only
the workspace, the host verifies and imports, the cache port stays
GET-only. Residual, stated not hidden: an input-addressed path is admitted
on the agent's word — the signed receipt re-attests it under the host key
but does not rebuild it (the design's strict mode, mitigation 3, is the
remaining step). The podman run of the design's acceptance pair is T-A
(consumer: offline realize, zero copies) + T-B (producer roundtrip: push →
auto-ingest → fresh sandbox, zero downloads), both measured 2026-10-26;
`./acceptance-dedup` reruns the whole pair from scratch with its own
cleanup.

Measured gotchas (each hit during bring-up, each workaround baked in):

| observation | consequence |
|---|---|
| `--load-db` opens top-level store files `O_NOFOLLOW` | symlinked drvs → `ELOOP`; copy files, symlink dirs |
| `--load-db` requires every row's file present | no "db-only" seed; the mirror must exist |
| `/proc/sys/fs/protected_hardlinks=1` | hardlink dedup refused (neither end owned) |
| tmpfs over `/nix/store` hangs podman (rc=124) | use directory binds |
| workload uid → host 100999 vs workspace owner 1000 | workspace writes need the step-7 ACL |
| `nix` absent from sandbox PATH | helpers resolve `/nix/store/*-nix-*/bin/nix` |
| stale state dirs are uid-mapped | `podman unshare rm -rf` to clean, never wipe in place |
| phase `Ready` precedes `seed: done` by ~6–20s (load-db checks every derivation) | `sandbox exec` probes race a partial db → wait for the seed line (acceptance `wait_seed`); wrapped workloads are guarded |
| the ingest unit runs the *installed* `~/.local/libexec/openshell/ingest.sh` | reinstall after every edit — a stale copy silently ran the old script (measured) |
| interrupted `--load-db` leaves an empty `db.sqlite` | the `.seed-ok` marker makes the next run reload it instead of skipping |

### Migration: shared read-only store (old default) → private dedup store

Since 2026-10-26 `run-sandbox` defaults to `PRIVATE_STORE=1`. What changes,
and what to do about it:

| before (shared ro) | now (private, default) |
|---|---|
| `/nix/store` shared read-only: workloads could only run what the host already had; profile installs and builds died with EROFS | private writable store: host paths pre-valid via the mirror + db seed, writes land locally |
| sandbox create returned immediately | staging on every create (~10–20s: mirror links, ~27MB of top-level file copies, db dump) |
| mounts: `/nix`, `/etc/nix`, `/workspace` (+fnm) | three more: private `/nix/store`+`/nix/var` dirs, read-only `/opt/host-store` mirror, ro `/opt/seed` |
| guest `nix-collect-garbage` had nothing it could delete | guest GC can only collect guest-created files — mirror entries are immutable symlinks into the host store |

* **Opt out:** `PRIVATE_STORE=0 ./run-sandbox` — the exact previous shape
  (shared ro `/nix`, no staging, no seed).
* `SHARE_NIX_RW=1` (the old escape hatch that re mounts `/nix` read-write)
  is orthogonal and unchanged.
* **Disk/RAM:** stage dirs land under `/tmp` (`TMPDIR=…` relocates them).
  On this machine `/tmp` is disk-backed; on a tmpfs `/tmp` the ~29MB mirror
  *and every build output* consume RAM — stage on disk there. Orphaned
  stages (after the owning sandbox is deleted): `podman unshare rm -rf
  /tmp/openshell-store.*`.
* **Scripts** that treated `ls /nix/store` as "what the host has" should
  read `/opt/host-store` instead (the read-only host mirror; `/nix/store`
  is now the sandbox's own view of it plus its private writes).
* **Semantics unchanged:** `/nix` itself stays mounted read-only and the
  policy's Landlock allowlist is untouched — the default only swaps what
  `/nix/store` and `/nix/var` *point at*.

## Acceptance: `./acceptance-dedup`

The design's acceptance pair (§7) as **one rerunnable check**: no
arguments, scrubs leftovers from crashed runs beforehand, cleans up after
itself (sandboxes, uid-mapped stage dirs, the test store path, its gcroot
and receipt), and exits non-zero on any FAIL. Run it after changing
anything in this folder.

```sh
./acceptance-dedup          # ~5 min; per-run logs under /tmp/acceptance-dedup.*
```

What it asserts, in order:

* **pre-clean** — stale sandboxes and stages from crashed runs are scrubbed
  first, so a rerun cannot inherit poisoned state;
* **T-A (consumer)** — a fresh `PRIVATE_STORE=1` sandbox: Ready *and* seed
  complete (it waits for `seed: done`; probing earlier reads a
  partially-loaded db), mirror shape (≥4000 entries, directories symlinked,
  no dir content copied, <500MB total), all three path shapes valid
  (drv / file output / dir output), a closure query over seeded refs, and
  an **offline realise** (`--option substituters ''`) returning rc=0 in
  ~110ms with **zero** new store entries and zero new files;
* **T-B (producer)** — guest A creates a new path and `nixcache-push`es
  it; the host path unit auto-ingests it (import + gcroot + signed narinfo
  served + archived signed receipt + inbox consumed); a **fresh** sandbox
  B then finds the path valid at seed time with `~/.cache/nix` absent
  (zero downloads) and matching content.

Measured 2026-10-26: **PASS — 25/25, 0 WARN** (the run before the
seed-wait and reinstall fixes was 18/5).

## Trust notes (same ones, restated for containers)

* `enable_bind_mounts` exposes host directories to sandbox workloads; the
  policy's Landlock allowlist is what keeps everything else closed, and
  `/nix` is mounted **read-only** — guests can read every store path (that
  is the point) and write none (`SHARE_NIX_RW=1` remains an escape hatch).
* `nix-serve` listens on `0.0.0.0:5000`, and served narinfos are signed with
  the machine's own `cache-1` key (the private key never leaves `~/.config/
  nix-serve/`, mode 600). The sandbox reaches it as
  `host.openshell.internal:5000` and nothing else — one rule, `protocol: tcp`,
  binaries `/nix/store/*/bin/nix` and `/nix/store/*/bin/nix-store`. The alias
  carries the load twice: podman copies the host's `/etc/hosts` into the
  sandbox, so a LAN name would resolve locally and never reach Policy DNS
  (and a bare IP is denied by design — `endpoint 10.0.0.108:5000 is not
  allowed by any policy`), while `host.openshell.internal` is absent from
  that hosts file, is documented as the name a policy may allow onto the
  gateway host, and is the name the supervisor maps back to it.
* Guest → host package flow: **implemented** for `PRIVATE_STORE=1`
  sandboxes (see "Deduplication" below) — `nixcache-push` writes a binary
  cache into the shared-workspace inbox, `nix-ingest.service` imports it
  into the host store and pins a gcroot, and `nix-serve` then serves it to
  every sandbox and host. `nix-serve` itself still has no upload verb:
  pushes ride the workspace share, never the cache port — the trust split
  of `../openshell-nixos/guest-cache-design.md` §4. The VM stack keeps the
  design-only status.

## Verification (measured 2026-10-26, each leg in a fresh sandbox)

The full loop (T-A + T-B) is checked end to end by
`./acceptance-dedup` — **PASS 25/25 on 2026-10-26**. The legs below are
the cache/policy measurements:

```sh
NIX=/nix/store/b3bifxbf3qsgsvqgr6lwj376favs8hlp-nix-2.34.9/bin/nix   # from the shared store
P=/nix/store/gs852k7fxjnc2y81xhzi44gpz3iy2qd7-dns-root-data-2025-04-14

# A. signed narinfo from the cache — 6/6 runs rc=0, signature present
./run-sandbox -- "$NIX" --extra-experimental-features nix-command \
    path-info --store http://host.openshell.internal:5000 --json "$P"
#   "signatures":["cache-1:4SOoylWNNXInYHOapHTJq5DpaHCZg7r8w0cCIaBUjUDb37yC9dB8ZCYr39RhrQFS4crhlDiTtmA4ne6KheFrDg=="]

# B. a NAR pulled through the cache into an empty store — 4/4 runs rc=0
./run-sandbox -- sh -c "$NIX --extra-experimental-features nix-command \
    copy --from http://host.openshell.internal:5000 \
    --to 'local?root=/tmp/rs' $P && find /tmp/rs -name '*dns-root-data*'"
#   copying path '/nix/store/gs852…-dns-root-data-2025-04-14' from 'http://host.openshell.internal:5000'...
#   /tmp/rs/nix/store/gs852k7fxjnc2y81xhzi44gpz3iy2qd7-dns-root-data-2025-04-14

# C. the same binary, endpoints the policy does not name — both rc=1
./run-sandbox -- "$NIX" ... store ping --store http://10.0.0.108:5000    # bare IP
./run-sandbox -- "$NIX" ... store ping --store http://cache.nixos.org    # unapproved
```

The event trail (`openshell logs <name>`) for B and C:

```text
NET:OPEN  ALLOWED /nix/store/b3bifxbf…-nix-2.34.9/bin/nix(0) -> host.openshell.internal:5000 [policy:nix_cache engine:opa]
HTTP:GET  ALLOWED GET http://host.openshell.internal:5000/nix-cache-info
HTTP:HEAD ALLOWED HEAD http://host.openshell.internal:5000/gs852….narinfo
HTTP:GET  ALLOWED GET http://host.openshell.internal:5000/nar/gs852…-1svg8w….nar

DENIED /nix/store/b3bifxbf…/bin/nix(0) -> 10.0.0.108:5000 [reason:transparent_tcp_policy_denied]
  reason=endpoint 10.0.0.108:5000 is not allowed by any policy
NET:REFUSE [MED] DENIED cache.nixos.org [reason:policy_dns_ineligible]
DENIED /nix/store/b3bifxbf…/bin/nix(0) -> cache.nixos.org:80 [reason:transparent_tcp_policy_denied]
```

### The flake this uncovered (fixed in `nix-serve.service`)

Before the fix, about one run in three failed with
`Weird server reply (8) Invalid status line` on the narinfo — 3 failures in
9 runs. The cache was the culprit, not OpenShell: a raw socket straight to
`nix-serve` (no sandbox, no supervisor) shows **Starman 0.4018 answering a
HEAD with a full body** — 453 bytes after the headers, every time. Starman
consults `REQUEST_METHOD` only when choosing chunked encoding and then
writes `$res->[2]` unconditionally. nix issues a HEAD before each narinfo
GET, so the stray body lands exactly where the client expects the next
status line; whether the connection is recycled first is a race, hence the
intermittency — and a failed lookup disables that substituter for 60 s.

The unit now passes `--disable-keepalive`, so the connection closes after
every response and the stray bytes are discarded with it: 6/6 consecutive
A runs and 4/4 B runs passed (against 3-in-9 before). The trade is one TCP
connect per request against a LAN/localhost cache, which is noise.
