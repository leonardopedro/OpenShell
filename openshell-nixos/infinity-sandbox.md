# Infinity in a cloud-hypervisor microVM, baked through the shared Nix store

`infinity-sandbox` runs [Infinity Code](../../infinity) as the workload of an
OpenShell sandbox on the fork's **cloud-hypervisor** VM backend. Infinity is not
uploaded into the sandbox and no image is rebuilt to upgrade it: the three
binaries are added to the host's Nix store, the store is mounted read-only into
the guest at the same path (VirtioFS + DAX, the mechanism described in
[README.md](README.md)), and the guest executes
`/nix/store/<hash>-infinity/bin/infinity` directly out of the host's copy.

```text
host                                        guest (cloud-hypervisor microVM)
┌───────────────────────────────┐           ┌──────────────────────────────────┐
│ openshell-gateway             │           │ openshell-sandbox (PID 1)        │
│  └ openshell-driver-vm        │─ virtiofs ▶  /nix        (ro, = host store)   │
│       (cloud-hypervisor, KVM) │           │   └ /nix/store/<hash>-infinity   │
│ /nix/store/<hash>-infinity    │           │        /bin/{infinity,           │
│ ~/.edge0/api-key              │           │             infinity-provider-   │
│ edge0-infer serve :8000/8001  │◀──────────│             openai, sandbox-     │
│                               │  vsock /  │             local}               │
│                               │  policy   │  /workspace (rw, = $PROJECTS_DIR)│
└───────────────────────────────┘           │   └ .infinity-home  (guest HOME) │
                                            └──────────────────────────────────┘
```

Two mounts are used (x86_64 allows three per sandbox): `/nix` read-only and
`$PROJECTS_DIR` read-write at `/workspace`. `ripgrep` and `git` for the guest
come out of the same store mount, so they cost no mount either.

## Prerequisites

| requirement | check |
| --- | --- |
| Gateway installed and running (`./install.sh`) | `openshell gateway list` |
| KVM access | `id -nG \| grep -qw kvm` |
| `virtiofsd` on the gateway's `PATH` | `command -v virtiofsd` (or `nix shell nixpkgs#virtiofsd`) |
| cloud-hypervisor binary | see below |
| guest kernel (bzImage or ELF vmlinux) | see below |
| `nix` | `nix --version` |

The driver finds the VMM through `OPENSHELL_VM_CH_PATH` and the guest kernel
through `OPENSHELL_VM_CH_KERNEL`. Both are read by the **gateway/driver**
process, so put them in `~/.config/systemd/user/openshell-gateway.service`
(and `systemctl --user daemon-reload`), not just in your shell — a sandbox
launched from a shell does not inherit your shell's environment into the
gateway.

Building the patched VMM the fork uses (SpectrumOS GPU/virtiofs patches) is
`../../australVM/cloud_hypervisor_vm/setup.sh`; it writes
`../../cloud-hypervisor-build/cloud-hypervisor/target/release/cloud-hypervisor`.
The pre-built `.deb` in that tree contains the same binary:

```sh
dpkg-deb -x ../../cloud-hypervisor-build/cloud-hypervisor-spectrum_50.0-spectrum0_amd64.deb /tmp/ch
# /tmp/ch/usr/bin/cloud-hypervisor
```

A stock upstream cloud-hypervisor works too (the GPU patches are only needed for
passthrough). The guest kernel is a separate download:

```sh
curl -Lo ch-vmlinux-x86_64 \
  https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20251112/vmlinux-x86_64
```

## Serve Edge0 for the sandbox

Sandboxes reach host services as `host.openshell.internal`, so the model server
must listen on a non-loopback address, and a non-loopback listener requires a
key file:

```sh
install -d -m 0700 ~/.edge0
head -c 32 /dev/urandom | base64 | tr -d '\n' > ~/.edge0/api-key
chmod 600 ~/.edge0/api-key

GGML_VK_VISIBLE_DEVICES=1 build/linux-vulkan/bin/edge0-infer serve \
  --model "$MOE_ASSETS_35B" --device vulkan --model-option expert_slots=24 \
  --host 0.0.0.0 --port 8001 --api-key-file ~/.edge0/api-key
```

`GGML_VK_VISIBLE_DEVICES=1` keeps the engine off a second Vulkan device (an
Intel iGPU is a hard failure, not a fallback); `expert_slots=24` fits the full
16K context in ~5 GiB of VRAM on a 6 GiB card. Use `--device cpu` with
`build/cpu/bin/edge0-infer` if there is no usable GPU — roughly 2.4× slower
decoding and 4× slower prefill. The CPU server listens on **8000** and pairs
with the `edge0-openai` profile; the GPU server listens on **8001** and pairs
with `edge0-gpu`, so both can run at once.

## Allow the sandbox to call it

The `edge0-openai` profile in [`../providers/edge0-openai.yaml`](../providers/edge0-openai.yaml)
authorizes `host.openshell.internal:8000` for the store-baked binary
(`/nix/store/*-infinity/bin/infinity-provider-openai`) plus the usual install
locations:

```sh
openshell provider profile lint   -f providers/edge0-openai.yaml
openshell provider profile import -f providers/edge0-openai.yaml
openshell provider create --name edge0 --type edge0-openai
```

For the GPU server on 8001, import
[`../providers/edge0-openai-gpu.yaml`](../providers/edge0-openai-gpu.yaml)
instead: identical binaries and credential model, endpoint on port 8001, id
`edge0-gpu`.

```sh
openshell provider profile lint   -f providers/edge0-openai-gpu.yaml
openshell provider profile import -f providers/edge0-openai-gpu.yaml
openshell provider create --name edge0-gpu --type edge0-gpu
```

## GPU profile (host-served Vulkan model)

`SANDBOX_GPU=1` makes the sandbox use the GPU-backed model:

```sh
SANDBOX_GPU=1 PROJECTS_DIR=~/code ./infinity-sandbox
```

It selects the `edge0-gpu` provider profile, defaults the provider's base URL
to `http://host.openshell.internal:8001/v1`, and runs four host-side preflight
checks before creating anything:

1. `nvidia-smi` reports a card and its used/total VRAM (24 slots + 16K context
   needs ~5 GiB free on a 6 GiB card);
2. an `edge0-infer serve` process exists;
3. that process was started with `--device vulkan` (a CPU server on 8000 would
   not match the profile's endpoint);
4. `http://127.0.0.1:8001/v1/models` answers with the key from
   `~/.edge0/api-key`.

Each failure is a warning, not an abort: the sandbox still boots, but model
calls fail, so the checks are what you read first when the agent gets no
answers. `EDGE0_GPU_PORT` changes the port, and `EDGE0_BASE_URL` still
overrides the URL outright.

**The GPU is not passed through into the guest.** The model keeps running on
the gateway host and the sandbox reaches it over vsock like any other host
service. That is deliberate: the OpenShell VM image ships no NVIDIA userspace,
so a VFIO card would appear in the guest with no usable Vulkan driver, and it
would pin the card out of the host's use for the sandbox's lifetime. With the
host-resident server, one loaded model serves every sandbox and the GPU is
never dedicated to one VM.

## Build and run Infinity

```sh
(cd ../../infinity && cargo build --release -p infinity-agent-cli \
  -p infinity-provider-openai -p sandbox-local)

PROJECTS_DIR=~/code ./infinity-sandbox
```

The launcher:

1. copies the three binaries into a directory named `infinity` and runs
   `nix store add-path` (content-addressed, so an unchanged build is free and
   an upgraded build becomes a new path — the old one stays valid for running
   sandboxes);
2. builds `ripgrep` and `git` from nixpkgs and puts their `/nix/store` bin
   directories on the guest's `PATH`;
3. stages the host's `~/.infinity` into `$PROJECTS_DIR/.infinity-home` as a
   hardlink farm and rewrites `providers.json` there to point at
   `http://host.openshell.internal:8000/v1` with the store-baked provider
   binary, plus `rap.json` registering `sandbox-local --enable-sandbox=false`
   (OpenShell's VM is the isolation boundary; Bubblewrap cannot nest inside the
   supervisor);
4. creates the sandbox with
   `driver_config.vm.backend = "cloud-hypervisor"`, the two mounts, the edge0
   provider attached, and `INFINITY_OPENAI_API_KEY` from `~/.edge0/api-key`
   passed with `--env-from` so the token never appears in a command line.

`HOME` inside the guest is `/workspace/.infinity-home`, so sessions, history and
any state writes land back in the mounted project directory on the host
(`.infinity-home` is rebuilt on every launch; remove it to reset agent state).

A bare task after `--` is translated to Infinity's `--initial-message`; any
flag is passed through unchanged, so a one-shot task that exits without the TUI
uses `--headless`:

```sh
PROJECTS_DIR=~/code ./infinity-sandbox -- "explain this repo"
PROJECTS_DIR=~/code ./infinity-sandbox -- --headless "explain this repo"
```

Useful knobs: `SANDBOX_GPU` (1 = host GPU model via `edge0-gpu`, see above),
`EDGE0_GPU_PORT` (default 8001), `SANDBOX_NAME`, `EDGE0_MODEL`,
`EDGE0_CONTEXT_WINDOW`, `EDGE0_BASE_URL`, `EDGE0_API_KEY_FILE`,
`INFINITY_BIN_DIR`, `INFINITY_TOOLS` (default `ripgrep git`, empty to rely on
the image), and `DRY_RUN=1` to print the `openshell sandbox create` command
without running it.

## Verification status

Checked here:

- `bash -n infinity-sandbox`, `DRY_RUN=1 ./infinity-sandbox` (prints the create
  command with the store path and mount JSON; a bare task becomes
  `--initial-message`, flags pass through).
- `nix store add-path` of the three binaries, and that the resulting path holds
  `bin/{infinity,infinity-provider-openai,sandbox-local}` (both debug and
  release builds).
- `cargo build --release -p infinity-agent-cli -p infinity-provider-openai
  -p sandbox-local` in `../../infinity`.
- `./check.bash` in `../../infinity` (all steps, with `jj` on `PATH`), and
  `cargo test -p infinity-provider-openai` (unit, binary and doc tests).
- Both provider profiles parse as YAML and pass `openshell provider profile
  lint` against a live gateway; the OpenShell CLI and gateway build from source.
- `tasks/scripts/vm/download-kernel-runtime.sh` now falls back to `curl` when
  `gh` is absent, so `vm-setup.sh` can stage libkrun/umoci without GitHub CLI.
  With the `x86_64-unknown-linux-musl` Rust target installed, `vm:supervisor`
  bundles the guest binaries and the source-built VM driver links them in.

Not checked here: an actual cloud-hypervisor boot of this launcher. The VM
driver itself boots fine with the packaged libkrun backend, and the full
Edge0 → Infinity provider → Infinity harness chain was verified inside that
sandbox. The cloud-hypervisor backend is blocked on this host by binary
compatibility, not by this repo: the fork-built CH v50 aborts with
`fatal runtime error: IO Safety violation` after virtiofsd disconnects;
upstream v49 cannot connect the vhost-user socket; v54 rejects the driver's
config (`Validation(ImageTypeRequired)`). virtiofsd 1.14 also warns it cannot
set root uid/gid and must disable file handles as an unprivileged user. Use a
matching CH/virtiofsd pair (or a privileged gateway) before relying on this
backend.

If a guest command fails with `EACCES` on `/nix` or `/workspace`, the Landlock
policy in [`sandbox-policy.yaml`](sandbox-policy.yaml) is the place to look; a
denied model call means the provider profile's `binaries` entries do not match
the store path (uploaded binaries need `/sandbox/bin/infinity-provider-openai`,
now listed in both profiles).
