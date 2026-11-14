# openshell-nixos — NVIDIA OpenShell on NixOS, with the host store shared into the microVM

Installs and configures **[NVIDIA OpenShell](https://docs.nvidia.com/openshell)**
(v0.1.5) on this NixOS machine so coding agents — **Freebuff first, in
production** — run inside libkrun microVM sandboxes that *share* the host's
files instead of copying them: one physical copy of the Nix store on disk,
one copy of the node/freebuff toolchain. RAM deduplication is measured,
not assumed: shared files live in **one** physical RAM copy too — the
fork's driver (Route A) gives every virtiofs share a DAX window and the
guest mounts them `dax=always`, so guests map the host page cache
instead of copying it (verified below; stock v0.1.5 exposes no DAX knob,
which is exactly what the fork adds). ksmd still cannot contribute (it
only merges memory that registers itself with madvise, and never
file-backed pages anyway — see "KSM reality check" below).

This directory is deliberately **independent of `../unfer` and
`../australVM`** (separate folder, no code vendored). The sharing mechanism
is their idea, credited below.

```text
host (NixOS)                                guest (libkrun microVM, per sandbox)
┌──────────────────────────────┐            ┌────────────────────────────────┐
│ openshell-gateway (user svc) │            │ openshell-sandbox  (PID 1)     │
│  └ openshell-driver-vm       │── virtiofs ─▶  /nix           (ro, = host)  │
│       (libkrun, KVM)         │            │  /var/lib/fnm   (ro, = host)  │
│ /nix/store   ← one copy (disk)│            │  /workspace     (rw, ~/Proj…) │
│ /var/lib/fnm ← one copy (disk)│            │  freebuff runs from the       │
│ host page cache serves reads  │◀─ DAX ───▶│  maps host frames, no copy    │
│ ksmd: on (madvise-only)      │            │  its own overlay + /workspace │
└──────────────────────────────┘            └────────────────────────────────┘
```

## Install (the sudo step)

```sh
sudo ./install.sh            # add --dry-run to preview, --help for options
```

What it does, in order:

1. **Preflight** — requires NixOS, `/dev/kvm`, the invoking sudo user;
   reports kvm-group status and where `mke2fs`/`debugfs`/`e2fsck` resolve.
2. **System module** — renders `/etc/nixos/openshell.nix` from this
   folder's `nixos-module.nix` template: the package (a *locked*
   `builtins.getFlake` ref — path + content hash — to this folder's build
   of the official v0.1.5 `.deb`), the KSM service, the kvm group and
   loginctl linger. Rewritten only when the render differs, and never
   over a file whose `managed-by` marker you removed by hand.
3. **Track + wire** — `git add`s the module if needed (Nix evaluates a
   git worktree through git, so untracked files are invisible to flake
   eval), then adds `./openshell.nix` to the system flake's modules list,
   anchored on its own `./x.nix` convention (idempotent; `--no-wire`
   skips it on reruns; a `flake.nix.bak.openshell` backup is kept).
4. **Switch** — `nixos-rebuild switch --flake /etc/nixos`: one new system
   generation carrying the package, `ksm-sharing.service`, the kvm group
   and linger (the package ref is content-locked inline via its narHash —
   `flake.lock` is not touched).
   nixpkgs' own `openshell` package is not used because it is CLI-only
   and stuck at 0.0.116 (schema v1 — it would reject this config).
5. **Profile cleanup** — removes the leftover `nix profile install`
   entry from this folder's earlier imperative recipe, if present.
6. **`~/.config/openshell/gateway.toml`** — rendered from `gateway.toml`
   (kept if present unless `--force-config`), then **gated on the real
   `openshell-gateway config preflight`** — an unknown key or schema
   change fails the install before anything starts.
7. **User unit** — `~/.config/systemd/user/openshell-gateway.service`,
   adapted from the unit upstream ships in the `.deb` (Apache-2.0), with
   store-absolute paths, a deterministic `PATH` (so the VM driver finds
   `mke2fs`/`debugfs`), cert generation, and `config preflight` as
   `ExecStartPre`.
8. **Start** — start `user@$UID` (linger itself now comes declaratively
   from the module), `systemctl --user enable --now openshell-gateway`,
   wait for `https://127.0.0.1:17670`.
9. **Register** — `openshell gateway add https://127.0.0.1:17670 --local
   --name local` + select, as your user.

Every system-level change lives in **`/etc/nixos`** — the new
`openshell.nix` module plus one line in `flake.nix`. Review them with
`git -C /etc/nixos diff` and commit them your way (the module is staged
by `git add`, so a `git commit -a` picks it up). Why there and not the
FHS-idiomatic `/etc/systemd/system`? On NixOS that directory is a symlink
into the immutable Nix store: an imperative unit install fails there by
design — which is exactly how this installer's first draft died
tee: read-only file system, after the config preflight had already
passed.

## Run Freebuff in the sandbox (production)

```sh
./freebuff-sandbox                     # freebuff in a microVM, your ~/Projects mounted rw
./freebuff-sandbox "fix the failing tests"   # args pass through to freebuff
PROJECTS_DIR=~/code SHARE_NIX_RW=1 ./freebuff-sandbox
```

Three mounts, which is exactly the x86_64 limit (3/sandbox; arm64 allows 32):

| host | guest | mode | why |
|---|---|---|---|
| `/nix` | `/nix` | ro | every nix package + shared library, content-addressed: what the host has, the guest already has |
| `/var/lib/fnm` | `/var/lib/fnm` | ro | the one node install carrying `freebuff`, `opencode`, `repomix` |
| `~/Projects` | `/workspace` | rw | the code the agent edits |

Deduplication on the **disk** axis: no closure is baked into the guest
image and no toolchain is copied in. On the **RAM** axis: each virtiofs
share gets a DAX window (`OPENSHELL_VM_DAX_WINDOW_MIB`,128 MiB per share
here) and the guest mounts it `dax=always`, so guest reads map the host's
page-cache frames instead of copying them — measured at **0 kB** guest
`Cached` growth on a126 MiB read (the pre-DAX control on this machine:
+146168 kB, see "KSM reality check"). ksmd merges neither the host's
file-backed cache nor libkrun's guest RAM, so it stays a secondary
mechanism. Freebuff's binary and state (login, history) are not in the
overlay either: the launcher hardlinks the host's `~/.config/manicode`
into `$PROJECTS_DIR/.freebuff-home` and points the guest's `HOME` at it,
so the guest execs the host's own binary and its state writes land on
the host's real files — rebuilt from the host on every launch.

First run: the driver pulls `nvcr.io/nvidia/base/ubuntu:24.04` by digest
(no Docker needed), boots (10–30 s), and OpenShell will ask you to approve
freebuff's network egress through its policy advisor — answer once for a
production-worthy allowlist. Freebuff itself is never downloaded inside
the sandbox: the launcher stages your host's `~/.config/manicode` into
the workspace share (the hardlink farm above) and aborts up front, with
instructions, if the host binary is missing.

## Security decisions (read before weakening)

* **`/nix` is read-only.** `SHARE_NIX_RW=1` flips it (and the config's
  `read_only`) only if you accept the boundary OpenShell itself states:
  *libkrun does not confine a compromised guest kernel to the configured
  source directory* — a writable `/nix` means a sandbox can corrupt the
  host's package store, and all bets are off. This mirrors the decision
  `../unfer/unfer_nixvm` explicitly left open; here the safe default is
  the one that ships.
* **Bind mounts bypass Landlock/workspace isolation.** OpenShell says so
  in its own warning; that is why the three opt-ins
  (`allow_driver_config`, `enable_bind_mounts`, resource admission off)
  exist and why this folder mounts *only* the three paths above. Think
  before adding mounts — on x86_64 you cannot add a fourth anyway.
* **The rw `/workspace` mount is the real trust boundary**: uid 1000 in
  the guest matches your uid, so the sandbox can rewrite your working
  trees (that is the point) — but not your home beyond it, not `/etc`,
  not `/nix`. The `.freebuff-home` hardlink farm deliberately places
  your freebuff state inside that boundary (sharing it is the point; it
  is rebuilt from `~/.config/manicode` on every launch — remove the
  folder to revoke it).
* **The gateway is loopback-only** (`127.0.0.1:17670`, TLS with a
  locally generated CA) and runs as *your user*, not root.
* **Guests have no network interface at all** — DNS/egress go through the
  host supervisor under OpenShell policy. VM sandboxes cannot use GPU
  passthrough and mounts at the same time (upstream rule); this setup
  uses mounts, not `--gpu`.

## Provenance

* The **shared-store mechanism** — mount the host's `/nix` into the guest
  so *the host store IS the guest store* (content-addressed, immutable,
  no copy), with the read-only default and the documented `ro`→`rw`
  trade-off — is the work already done in **`../unfer/unfer_nixvm`**
  (flake composition; MIT OR Apache-2.0) and
  **`../australVM/cloud_hypervisor_vm`** (the `configuration.nix`
  `fileSystems."/nix"` virtiofs recipe + launch script bug-fix;
  Apache-2.0/BSD-3-Clause). Those used cloud-hypervisor + virtiofsd;
  OpenShell's VM driver provides the same virtiofs share through
  **libkrun**, so this folder ports the *mechanism*, not code. It also
  repeats one of their lessons explicitly: every path here is absolute
  (the relative-`--shared-dir` bug their README documents).
* **NVIDIA OpenShell** artifacts and unit file: Apache-2.0, downloaded
  checksum-verified from the official `v0.1.5` GitHub release; the
  systemd unit is adapted from the one in the `.deb`.
* **KSM**: kernel facility, enabled by the `ksm-sharing` service now
  declared in `openshell.nix` (same three commands the recipe ran, plus
  `use_zero_pages`).

## Verification status (2026-10-23, written by the agent)

Checked here, before delivery:

* `nix build .#openshell` — **0 unsatisfied autoPatchelf dependencies**;
  `openshell --version` → `0.1.5`; gateway/driver/prover all exec.
* Rendered `gateway.toml` — **`openshell-gateway config preflight` exit 0**
  (schema v2 keys confirmed against the real binary, including
  `allow_driver_config` / `enable_bind_mounts` / resource admission).
* `generate-certs` runs and writes the local PKI.
* **System-config evaluation** — the exact artifacts this installer writes
  (module rendered with the real `narHash`, user and KSM values;
  `./openshell.nix` wired with the same anchor `sed`) were produced on a
  copy of this machine's real `/etc/nixos` flake, then
  `nix eval …#nixosConfigurations.nix2.config.system.build.toplevel.drvPath`
  → **exit 0**: the `getFlake` package ref, the `ksm-sharing` service, the
  `extraGroups` merge and `linger` all type-check inside the full
  home-manager + nix-amd-ai composition, and
  `environment.systemPackages` contains `/nix/store/hsn52…-openshell-0.1.5`
  — byte-identical to this folder's own verified build.
* The `render_module` function under test (both `--no-ksm` values, both
  kvm-group branches, no leftover `@TOKEN@`), the wire `sed`'s anchor
  (inserts after `./configuration.nix` with matching indentation),
  `bash -n install.sh freebuff-sandbox`, and `./install.sh --dry-run`
  exit 0 through all nine steps — plus the `--no-ksm --no-wire` branches.

**Live-verified on the real machine (2026-10-23, after `sudo ./install.sh`):**

* **Gateway** — `openshell-gateway` user service `active`, mTLS-connected to
  `https://127.0.0.1:17670` (`openshell gateway list` shows it selected).
* **Declarative unit wins** — the leftover imperative unit (which pointed
  `ExecStartPre` at a removed `~/.nix-profile` path) was replaced by the
  template's store-absolute one; a stale live copy was repaired and the
  next `install.sh` regenerates it byte-identically (idempotent `cmp`).
* **First microVM boots** — the full pipeline works: image pulled from
  `nvcr.io` by digest (512 MB, cached), VM supervisor up in ~1.5 s, guest
  command executed through all three virtiofs mounts.
* **The three mounts are live and correct** — `/nix` (ro) lists 21 126
  store entries; `/var/lib/fnm` (ro) runs `node` **v26.10.0**; `/workspace`
  (rw) writes/reads/cleans a file that is visible on the host, owned by
  uid 1000. `openshell sandbox get` reports `WorkloadIdentity … 1000:100`
  — the uid pinning in `gateway.toml` did its job.
* **`sandbox-policy.yaml` is the Landlock half of the mount fix** — the
  mounts attach without it, but the built-in default policy keeps them
  outside the allowed path set, so every `open()` returned `EACCES`. With
  `--policy`, all three are readable/writable as declared.
* **`freebuff-sandbox` launcher fix (LD_LIBRARY_PATH)** — node is a
  nixpkgs build under fnm, so it links `libatomic`/`libstdc++` from
  `/nix/store`; the plain-Ubuntu guest has no nix-ld to find them, and
  node died with `libatomic.so.1: cannot open shared object file`. The
  launcher now resolves the store's gcc-lib dir on the host (via nix-ld's
  symlink farm, so no store path is hard-coded) and exports it in the
  guest command; verified live (node runs, freebuff wrapper execs).
* **freebuff itself launches — with zero downloads in the sandbox.**
  Re-verified after the workspace-farm change: the guest execs the
  host's own `~/.config/manicode/freebuff` (staged as hardlinks under
  `$PROJECTS_DIR/.freebuff-home`, `HOME` redirected there), the TUI
  renders with the host's project list, live-session markers appear
  through shared inodes, `--version` exits 0 printing `0.2.34`, and the
  log contains no `EACCES`/download attempt at all. Its API egress still
  stops at OpenShell's policy until approved — the documented first-run
  policy-advisor step, not a mount defect.

**KSM reality check (measured on this machine, kernel 7.2.9):**

* `ksm-sharing.service` is live and declarative: `active (exited)`,
  enabled, writing `run=1` + `use_zero_pages=1`, and after tonight's
  switch the unit, `/etc/nixos/openshell.nix` and the repo template all
  agree byte-for-byte.
* **What KSM does *not* do here:** merge microVM guest RAM. Two facts,
  both measured: (1) the kernel docs define `run` as "1 = run ksmd",
  "2 = stop ksmd and unmerge all pages" — there is no always-merge mode,
  so madvise registration is the *only* scanning contract; (2) with two
  live guests resident, `pages_scanned=0` over full scans, because
  libkrun never calls `madvise(MADV_MERGEABLE)` on guest RAM (QEMU only
  does with an explicit `mem-merge` flag; libkrun exposes none). ksmd
  scans only what registers itself — nothing in this stack does.
* An earlier note in this README claimed `run=2` would force merging;
  that was wrong (the docs' run=2 is stop+unmerge) and the live system
  agreed: `run=2` parked ksmd at 0 CPU with `full_scans` frozen. The
  template ships `run=1` — ksmd active and harmless, waiting for any
  madvise-registering workload.* **Host↔guest RAM duplication — the control, measured 2026-10-23 with
  stock v0.1.5: a shared file sat in RAM twice.** Test: a142.8 MB
  `/nix/store` file (node) was made fully resident in the host page cache
  (`fincore`:36551/36551 pages), then read inside a fresh guest. The
  guest's `Cached` grew by146168 kB — the file's exact size
  (36551 ×4 KiB ≈146177 kB) — while the host still showed
 36551/36551 pages resident: **two physical copies, simultaneously.**
  That is virtiofs without DAX: the fs server serves each read from the
  host page cache, then the bytes are *copied* into the guest's own page
  cache. (Boot-from-cache ~1.5 s and21126 store entries readable with
  no disk growth remain true — that is *disk* dedup, unaffected.)
* **Route A fixes it in this fork (implemented and measured 2026-10-23).**
  The fork's `openshell-driver-vm` takes `--dax-window-mib` /
  `OPENSHELL_VM_DAX_WINDOW_MIB` (128 here), passes it as libkrun's
  `krun_add_virtiofs4` `shm_size` ("size of the DAX SHM window in bytes",
  verified in the pinned v1.19.6 source), tags each share in the guest
  mount manifest, and the guest init mounts those shares `dax=always`.
  Verified live on this machine: all four virtiofs mounts show
  `dax=always`; reading a126894 KiB store file grew guest `Cached` by
  **0 kB** (vs +146168 kB control above), the host held
  **31724/31724 pages** resident, and the guest received the exact byte
  count. **N=2 — the scaling claim:** two fresh sandboxes (`/nix`
  mounted only) each read the same126894 KiB file — guest `Cached`
  growth **0 kB** and **32 kB** (0.025 % of the file) with guest
  `MemFree` flat (−248 / −16 kB), and host `MemFree` sampled seconds
  before/after each read never dipped by the file size (**+8.5 MB /
  +3.2 MB** — a per-guest *copy* inside the DAX window would have burned
  ~127 MB of host free pages or guest RAM per reader). Residency after
  all reads: still **31724/31724 pages**. So host + N guests reading the
  same file cost **one physical copy total** — guest↔host *and*
  guest↔guest deduplicated — where stock v0.1.5 cost 1 + N. (Sys-wide
  host memory churns by GBs over minutes from unrelated processes; only
  the seconds-apart before/after pairs are quoted here.) Stock v0.1.5
  still exposes no knob — `upstream-dax-issue.md` is
  the paste-ready request for that; on this machine the fork replaces it.
* **ksmd could not fix host↔guest duplication even if fully enabled**:
  the kernel docs are explicit that KSM merges only anonymous pages,
  *never* file-backed pages — the host side of a shared package is page
  cache, structurally outside KSM's reach whatever `run` value or
  madvise registration exists. KSM could only ever dedup guest↔guest
  copies, and only after something calls `madvise(MADV_MERGEABLE)`
  (libkrun doesn't).
* Mitigating shape: the duplication is reclaimable **cache**, not pinned
  memory — the host page cache is evicted under memory pressure, the
  guest's copy is evictable within the guest's own memory limit, and
  only files the guest actually reads duplicate.
* KSM *would* pay off the moment something madvises (a guest agent, or a
  `madvise(MADV_MERGEABLE)` helper over `process_madvise(2)` targeting
  libkrun's RAM region) — until then, expect `pages_shared=0` while
  `full_scans` climbs, and treat that as correct, not as breakage:
  `grep -H . /sys/kernel/mm/ksm/{run,pages_scanned,pages_shared,full_scans}`

**Not run** (deliberately — needs your sudo, your KVM, your login session):
the actual `sudo ./install.sh` — i.e. the real `/etc/nixos` writes, the
`git add`, and the first `nixos-rebuild switch` (the eval above proves it
evaluates; only your run builds and activates it) — then the first
gateway boot, the first sandbox boot, and the first freebuff session.
That is your step.

## Troubleshooting

```sh
journalctl --user -u openshell-gateway -n 50 --no-pager   # gateway
openshell status                                          # CLI <-> gateway
openshell sandbox list                                    # sandboxes
grep -H . /sys/kernel/mm/ksm/{run,pages_scanned,pages_shared,full_scans}  # ksmd live? (pages_shared=0 is normal unless something madvises)
systemctl status ksm-sharing                              # KSM unit (system)
git -C /etc/nixos diff                                    # what this install changed
```

* *Gateway refuses config* — the preflight message names the key;
  rerun `sudo ./install.sh --force-config` to reset to the template.
* *Driver not found* — `command -v openshell-driver-vm`; the gateway
  looks next to its own binary, so both must come from the same place.
  With this install both live together in `/run/current-system/sw/bin`,
  which the unit's `PATH` pins first. If `command -v openshell` finds
  nothing at all, the switch didn't land: rerun `sudo ./install.sh` (or
  `nixos-rebuild switch --flake /etc/nixos` and read its output).
* *First sandbox fails pulling the image* — needs plain outbound HTTPS to
  `nvcr.io`; disk here is at 94 %, and the image cache +
  `overlay_disk_mib` land in `~/.local/state/openshell/vm-driver`
  (`df -h` before long runs).
* *freebuff egress denied* — approve the endpoints in OpenShell's policy
  advisor on first use, or edit the policy; that is the sandbox working
  as intended.
* *Mounts rejected* — x86_64 caps at 3 per sandbox, targets may not be
  `/tmp`, `/proc`, `/dev`, `/srv`, OpenShell paths, the workspace root,
  or nested; symlinks in the path are refused.
