Deduplicating shared libraries (`.so` files, libc, Python runtimes, etc.) in RAM across multiple microVMs is done using two primary techniques: **`virtiofs` with DAX** (the modern, high-performance approach) and **KSM (Kernel Samepage Merging)** (the legacy, brute-force approach).

---

### Method 1: `virtiofs` + DAX (The Modern & Best Approach)

This is the standard architecture used by modern container-microVM runtimes (such as Kata Containers, `libkrun`, and Cloud Hypervisor).

#### How it works:
1. **Host-Side Storage**: The base rootfs or shared library directory (e.g., `/usr/lib`, Python site-packages) lives as regular files on the host filesystem or inside an uncompressed image (like an `erofs` or `ext4` loop file).
2. **DAX (Direct Access) Window**: When the hypervisor boots the microVM, it exposes a special PCI memory window mapped to the host's `virtiofsd` daemon with the DAX feature enabled (`--cache=always` or `--dax`).
3. **Zero-Copy Guest Page Mapping**:
   * Inside the microVM, the guest kernel mounts the filesystem using `mount -t virtiofs ... -o dax`.
   * When a process inside MicroVM 1 calls `dlopen("libpython3.11.so")` or executes a binary, the Linux dynamic linker (`ld.so`) issues `mmap()` on that library file.
   * Instead of allocating guest RAM and copying disk blocks over virtio, **the guest kernel directly points its page tables to the host’s physical page cache** via the virtiofs memory window.
4. **The Deduplication Magic**:
   * Because MicroVM 2 does the exact same `mmap()` on the *same underlying host file*, **both microVMs are mapped to the exact same physical RAM frames** in the host's page cache.
   * The read-only `.text` sections (executable code) of `libc.so`, Python, etc., reside in physical RAM **only once**, regardless of how many microVMs are running.
   * If a process writes to a data segment (e.g., `.data` / `.bss`), standard `mmap(MAP_PRIVATE)` Copy-on-Write (CoW) triggers, creating a private page only for the modified bits.

**Advantages:**
* **Zero CPU overhead:** Deduplication is instantaneous at load time; no background scanning is required.
* **Instant VM boot times:** MicroVMs don’t need to read libraries from disk into RAM.

---

### Method 2: Host Kernel Samepage Merging (KSM)

KSM is a built-in feature of the Linux kernel (`ksmd`) that performs post-hoc memory deduplication at the raw page level.

#### How it works:
1. **Registration**: When the hypervisor (Cloud Hypervisor, QEMU, or `libkrun`) allocates guest RAM on the host using `mmap()`, it calls:
   ```c
   madvise(addr, length, MADV_MERGEABLE);
   ```
2. **Scanning**: The host kernel daemon `ksmd` wakes up periodically, scans memory pages marked as `MADV_MERGEABLE`, computes page hashes, and finds pages with identical content (e.g., identical 4KB blocks of `libc.so` loaded into both VMs).
3. **Merging with CoW**:
   * When `ksmd` finds two identical pages, it points both page table entries to a single physical page and marks it as **Copy-on-Write (CoW)**.
   * The redundant physical page is freed back to host RAM.
   * If either microVM tries to write to that page, a CPU page fault occurs, and the host kernel transparently duplicates the page again.

#### Why KSM is less favored today:
* **High CPU Overhead**: `ksmd` continuously burns CPU cycles scanning and hashing gigabytes of RAM.
* **Latency**: Deduplication is not instant; it can take seconds or minutes after VM boot for `ksmd` to find and merge pages.
* **Security Side-Channel Risks**: KSM makes systems vulnerable to cache-timing attacks (such as **Flush+Reload**). By measuring the latency of a memory write, a malicious sandbox can determine whether another sandbox is running the same binary or accessing certain code pages. Because of this, KSM is often disabled in multi-tenant enterprise environments.

---

### Summary: Which should you use for OpenShell?

If you want shared library deduplication in OpenShell:

* **Use `virtiofs` + DAX**: Since OpenShell's native `openshell-driver-vm` is built on `libkrun` (which relies heavily on `virtiofs`), configuring shared read-only base layers over `virtiofs` with DAX gives you instant, zero-CPU deduplication of all shared system libraries and Python dependencies with zero security side-channel risks.

---

## Measured verdict (nix2, kernel 7.2.9, OpenShell v0.1.5 — 2026-10-23)

The bar for this setup: **if the host and a guest are both using the same
nix package, it must occupy one physical RAM copy, not two.** Verdict
with stock v0.1.5: **not met** — by exactly the mechanism this document
recommends (Method 1), which stock cannot enable. **Met on this machine
since Route A** (the fork's DAX-enabled driver — measurement at the end
of this section).

**Experiment.** A 142.8 MB store file (node) was made fully resident in
the host page cache (`fincore`: 36 551/36 551 pages), then read inside a
fresh sandbox with `/nix` mounted. Result: the guest's `Cached` grew by
146 168 kB — the file's exact size (36 551 × 4 KiB ≈ 146 177 kB) — while
the host still showed 36 551/36 551 pages resident. **Two physical
copies, simultaneously.** That is virtiofs without DAX: the fs server
serves each read from the host page cache and *copies* the bytes into the
guest's own page cache. Duplication materializes for every file the guest
reads.**Method 1 was unavailable in stock v0.1.5 — hence Route A in this
fork.** The probed gaps (2026-10-23) were real: no `--dax` / `--cache`
flag on `sandbox create` or `openshell-driver-vm`; the mount schema
rejects `dax` verbatim; the gateway preflight rejects unknown `vm` keys;
the stock driver binary carried no `set_dax_window_size` symbol; guest
mounts showed plain `ro,relatime`. But libkrun's *legacy* API already
had the knob: `krun_add_virtiofs4(..., shm_size, ...)`, where
`shm_size` is documented as "size of the DAX SHM window in bytes" and
implemented in the pinned v1.19.6 runtime (verified in source). The
fork's driver now takes `--dax-window-mib` /
`OPENSHELL_VM_DAX_WINDOW_MIB`, passes it as `shm_size`, records a `dax`
column in the guest mount manifest, and the guest init mounts those
shares `dax=always`. Measured (2026-10-23,128 MiB window): every
virtiofs mount shows `dax=always`; a126894 KiB store file read in the
guest grew guest `Cached` by **0 kB** (control above: +146168 kB) with
host residency **31724/31724 pages** — one physical copy, the bar
above. Scaling verified the same day with **N=2**: two fresh sandboxes
(`/nix` mounted only) each read the same file — guest `Cached` +**0 kB**
and +**32 kB** with guest `MemFree` flat, host `MemFree` never dipping
by the file size around either read (a per-guest window copy would cost
~127 MB of host or guest memory per reader), residency still
**31724/31724**. Host + two guests = **one physical copy** —
guest↔host and guest↔guest deduplicated; stock would have been three.
Footnote kept from the original framing: even where DAX exists
upstream it must be turned on explicitly, and Cloud Hypervisor has
dropped virtiofs DAX entirely.

**Method 2 cannot satisfy this bar, even with every knob turned on.**
Two independent reasons, both measured:

1. *KSM merges only anonymous pages, never file-backed ones* (kernel
   docs). The host side of a shared package is page cache — structurally
   outside ksmd's reach whatever the `run` value. Host↔guest file dedup
   via KSM is impossible by design. KSM's best case here is guest↔guest
   (each guest's page-cache copy *is* anonymous from the host's view),
   which helps only multi-sandbox workloads.
2. *The registration premise of Method 2 does not hold for libkrun*:
   “when the hypervisor … allocates guest RAM … it calls
   `madvise(MADV_MERGEABLE)`” — measured `pages_scanned=0` across full
   ksmd scans with two guests resident. libkrun never registers guest
   RAM (no mem-merge option); QEMU does only with an explicit flag.
   Without a registration helper (`process_madvise(2)` over libkrun's
   RAM region, or a guest agent), KSM merges nothing at all.

**Practical shape of the duplication.** Reclaimable **cache**, not
pinned memory: the host page cache is evicted under memory pressure, the
guest's copy is evictable within the guest's own memory limit, and only
files the guest actually reads duplicate. The disk axis is untouched by
all of this — one store copy on disk, 21 126 entries, no image growth.

**Options, strongest first:** (1) **done here** — Route A in this fork
meets the bar on this machine; the paste-ready upstream request for
stock installs is `upstream-dax-issue.md`; (2) accept bounded,
evictable duplication of each guest's read working set (the stock
v0.1.5 position); (3) a registration helper enabling guest↔guest KSM —
helps only when several sandboxes run the same files, never
host↔guest.
