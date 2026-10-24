# Upstream feature request — draft for https://github.com/NVIDIA/OpenShell
#
# STATUS (2026-10-23, evening): superseded *on this machine* by Route A —
# the local fork's driver now passes the DAX window size to libkrun's
# krun_add_virtiofs4 shm_size and the guest mounts dax=always (measured,
# see README / dedup.md). This draft remains valid for STOCK v0.1.5
# installs, which still expose no DAX knob. Two technical notes before
# filing: (1) the recommended config shape below (dax_window_mib) is what
# the fork implemented, so the request can point at a working design;
# (2) the "new builder API" framing needs revisiting — the legacy
# krun_add_virtiofs4 already carries shm_size (the DAX window) in pinned
# v1.19.6, so an upstream fix could be as small as plumbing + docs.
#
# No existing DAX issue found (checked 2026-10-23); closest relative is
# #1509 (additional virtiofs shares). The repo also has a Feature Request
# discussion category: https://github.com/NVIDIA/OpenShell/discussions/categories/feature-request
#
# Paste the TITLE and BODY below.

**TITLE:**
[Feature Request] MicroVM: expose a virtiofs DAX window so shared mounts are not duplicated in guest RAM

---

**BODY:**

## Summary

Request an opt-in DAX window for virtiofs mounts in the MicroVM driver
(`compute_driver = "vm"`), e.g. `[openshell.drivers.vm] dax_window_mib = 2048`
plus a per-sandbox `vm` driver-config field, wired to libkrun's
`krun_fs_device_set_dax_window_size()` and a guest-side `mount -t virtiofs -o dax`.

Today every byte a sandbox reads from a shared mount (e.g. a host `/nix`
store shared read-only) exists **twice** in physical RAM: once in the host
page cache and once in the guest's own page cache. With DAX the guest maps
the host's page-cache frames directly and the copy disappears — one physical
copy regardless of how many sandboxes read the file.

## Measured duplication (why this matters)

Environment: OpenShell v0.1.5, Linux 7.2.9, KVM, three read-only virtiofs
mounts (`/nix`, `/var/lib/fnm`) + one rw workspace mount.

Method:

1. Fully resident a 142.8 MB file from the shared `/nix` store on the host:
   `fincore` reports **36 551 / 36 551 pages resident**.
2. Read the same file inside a fresh sandbox over virtiofs.
3. Re-measure both sides.

Result:

- guest `Cached` grew by **146 168 kB — the file's exact size** (36 551 × 4 KiB);
- host `fincore` still shows **36 551 / 36 551 pages resident**;

→ **two physical copies resident simultaneously**, and `N` sandboxes reading
the same files give `N + 1` copies. This is inherent to virtiofs without DAX
(the fs server serves reads from the host page cache, then the bytes are
copied into the guest page cache), not a bug — but for read-only shared base
layers (store closures, language runtimes, agent toolchains) the duplication
is pure overhead, and DAX is the standard fix (Kata/QEMU expose it as
`virtiofs_dax_size`; the virtio-fs DAX design maps guest page tables at the
host's page cache).

The disk side of sharing already works perfectly (one store copy, no image
growth) — RAM dedup is the missing half.

## What is missing in v0.1.5 (probed)

- no `--dax` / `--cache` flag on `openshell sandbox create` or
  `openshell-driver-vm`;
- the `vm` mount schema rejects it verbatim:
  `unknown field 'dax', expected one of 'type', 'source', 'target', 'read_only'`;
- gateway `config preflight` rejects unknown keys in `[openshell.drivers.vm]`;
- the bundled runtime (`~/.local/share/openshell/vm-runtime/0.1.5/libkrun.so`,
  dlopen'd by the driver) exports **no** `*dax*` / `*fs_device*` symbols
  (`nm -D`): it carries only the legacy v1 API
  (`krun_create_ctx`, `krun_add_virtiofs` … `krun_add_virtiofs4`,
  `krun_set_vm_config`, `krun_start_enter`);
- live guest mounts show `ro,relatime` — no `dax` flag.

## Upstream capability

`containers/libkrun` main ships the API:

```c
/** Set the size of the DAX (direct access) shared memory window.
 *  When set, the guest can memory-map files from the shared filesystem
 *  directly into its address space, avoiding data copies.
 *  If not set, no DAX window is allocated. */
void krun_fs_device_set_dax_window_size(KrunFsDevice handle, uint64_t bytes);
```

The one wrinkle: DAX lives on libkrun's **new builder API**
(`krun_fs_device_new` → `krun_fs_device_set_dax_window_size` →
`krun_device_manager_add` → `krun_vmm_builder_*`), while v0.1.5's driver uses
the legacy v1 flow, which upstream main has removed. So the work item is
either (a) migrate the driver's launch path to the builder API, or (b) add a
DAX setter to the bundled runtime's legacy API. Either way the driver-side
plumbing (config field → FFI → mount manifest → guest mount option) is small.

## Suggested shape

1. `[openshell.drivers.vm] dax_window_mib` + per-sandbox
   `--driver-config-json '{"vm":{"dax_window_mib":2048}}'`
   (driver-config passthrough already exists behind `allow_driver_config`).
2. Driver: create shared filesystems via `krun_fs_device_new[_read_only]`
   and set the window size when configured.
3. Bootstrap/guest: add `-o dax` to the virtiofs mounts whose tag has a
   window (window bounds the per-VM shared-mmap working set; x86_64 guest
   address space is ample).
4. Docs: note the sizing trade-off and that DAX window memory is shared with
   the host page cache (one copy), which is the point.

## Alternatives considered (and why they don't get there)

- **KSM**: cannot help host↔guest at all — the kernel merges only anonymous
  pages, never page cache, and the host side of a shared file *is* page
  cache. Additionally libkrun never calls `madvise(MADV_MERGEABLE)` on guest
  RAM: measured `pages_scanned=0` across full ksmd scans with two guests
  resident. KSM could only ever dedup guest↔guest, and only with a
  registration helper.
- **Switching VMMs (e.g. crosvm/QEMU)**: the driver already has a QEMU
  backend but bind mounts are explicitly libkrun-gated, and replacing the
  VMM would discard the mature image/bootstrap/supervisor transport work.
  Exposing the libkrun API that already exists is the small path.

## Environment

- OpenShell v0.1.5 (official .deb, built via Nix flake)
- `compute_driver = "vm"`, `allow_driver_config = true`, mounts-based
  sandbox (no GPU)
- Host: NixOS, Linux 7.2.9, KVM
- Guest: `nvcr.io/nvidia/base/ubuntu:24.04`
- libkrun runtime: `~/.local/share/openshell/vm-runtime/0.1.5/{libkrun.so,libkrunfw.so.5}`

## Acceptance test we would run

With DAX enabled on a shared read-only mount: host `fincore` of a store file
stays fully resident **and** guest `Cached` does **not** grow by the file
size when the guest reads it — i.e. the measured two-copy state above
collapses to one.
