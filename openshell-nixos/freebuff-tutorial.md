# Tutorial: running Freebuff inside OpenShell microVMs (NixOS / nix)

This walkthrough takes any machine where the **nix package manager** is
installed and this **OpenShell fork is cloned** — NixOS or any other
distro with nix — from zero to Freebuff running inside a libkrun microVM
with the host Nix store shared read-only into the guest.

Everything in the main path runs **as your user**: no `sudo`, no
`/etc/nixos` edits, no machine-specific absolute paths. Every command
below is relative to your clone root — nothing here is tied to one
machine's paths.

```sh
git clone <your-openshell-fork-url> OpenShell
cd OpenShell        # every command in this tutorial runs from here
```

What you get (same architecture as [README.md](README.md)):

```text
host (you)                                 guest (libkrun microVM, per sandbox)
┌──────────────────────────────┐           ┌────────────────────────────────┐
│ openshell-gateway (your user)│           │ openshell-sandbox  (PID 1)     │
│  └ openshell-driver-vm       │─ virtiofs─▶  /nix           (ro, = host)  │
│       (libkrun, KVM)         │           │  /var/lib/fnm   (ro, = host)  │
│ one copy of the store on disk│◀── DAX ───▶  /workspace     (rw, your code)│
└──────────────────────────────┘           │  freebuff runs here           │
                                           └────────────────────────────────┘
```

---

## 1. Prerequisites

| requirement | check | if missing |
|---|---|---|
| nix package manager | `nix --version` | install from nixos.org (on NixOS it is always present) |
| KVM | `ls -l /dev/kvm` | enable virtualization in firmware/BIOS, or on a VM enable nested virt |
| KVM access for your user | `id -nG \| grep -qw kvm` | **the only sudo in the main path:** `sudo usermod -aG kvm "$USER"`, then log out and back in |
| git | `git --version` | `nix shell nixpkgs#git -c git --version` |
| Node + Freebuff on the host | `command -v node freebuff` | see step 2 |
| disk space | `df -h .` | the guest image (~512 MB) plus overlays land in `~/.local/state` and `.cache` |

Nothing else is assumed: no Docker, no system-wide OpenShell install, no
`/etc/nixos` changes.

## 2. Node and Freebuff on the host (once, as your user)

The sandbox does **not** copy any toolchain: it mounts your host's node
directory read-only and runs Freebuff straight from it. So install once,
here:

```sh
# however you manage node — fnm, nix profile, nvm, your distro — then:
command -v node                       # must be on YOUR PATH
npm install -g freebuff               # one global install, shared by every sandbox
command -v freebuff                   # must sit next to node
```

`freebuff` must be in the **same directory** as `node` (that is what a
plain `npm -g` under that node gives you).

Then **run it once on the host**:

```sh
freebuff --version     # first launch downloads the native binary
```

That first run stages Freebuff's real binary (~140 MB) **plus all of its
state** — credentials, settings, history — into `~/.config/manicode`
(the wrapper's configDir: always `$HOME/.config/manicode`; its
`configDir` option is test-only and no environment variable overrides
it). Sandboxes never download: step 6 execs this exact binary, and the
launcher aborts up front if it is missing.

## 3. One-time shell setup (open a fresh shell; exports are per-shell)

Run this once per terminal — it puts the repo's dev CLI on `PATH` and
gives the gateway the only external library it links against (z3, taken
straight from the nix store, no system install):

```sh
export PATH="$PWD/scripts/bin:$PATH"

Z3_LIB="$(nix build --no-link --print-out-paths nixpkgs#z3.lib)/lib"
export LIBRARY_PATH="$Z3_LIB${LIBRARY_PATH:+:$LIBRARY_PATH}"
export LD_LIBRARY_PATH="$Z3_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

You also need the workspace's Rust toolchain to build the gateway and
driver. Use whichever applies:

```sh
nix develop        # the repo flake's pinned toolchain — first run can take a while
# …or, if you already have the toolchain from rustup, you are done: cargo --version
```

> All build/start commands below assume this shell (the exports plus a
> cargo on `PATH`). `openshell status` etc. work in any shell once the
> first build has happened.

## 4. Download the VM runtime artifacts (once)

`vm-setup` downloads the pre-built libkrun/libkrunfw/umoci + guest
bundle (~30 s) into `target/vm-runtime-compressed`. It needs `gh` and
`zstd`; nix provides both without installing anything system-wide:

```sh
nix shell nixpkgs#gh nixpkgs#zstd -c ./tasks/scripts/vm/vm-setup.sh
```

(If you prefer mise: `mise run vm:setup` runs the very same script.)

## 5. Start the gateway (your user, loopback only)

```sh
./tasks/scripts/gateway-vm.sh
```

The script does everything, all user-space:

* builds `openshell-gateway`, `openshell-driver-vm` and
  `openshell-supervisor` into `target/debug/` (first run takes a few
  minutes; this is where the `LIBRARY_PATH` export from step 3 matters);
* generates a local TLS/key material under `.cache/gateway-vm/`;
* writes the gateway config (`default_image`, driver state dir, …) and
  gates it through `openshell-gateway config preflight`;
* listens on `127.0.0.1:18081` (plaintext, loopback — a local dev
  gateway; override the port with `OPENSHELL_SERVER_PORT=…`);
* registers itself as gateway `vm-dev` and selects it for your CLI.

Verify in a second shell (same clone, `PATH` export from step 3 if you
did not export it there):

```sh
openshell status            # Server Status → Connected
openshell sandbox list      # empty, but proves CLI ↔ gateway
```

To make it start at login instead of living in a terminal, wrap it in a
`systemd --user` unit (still no sudo) — optional.

## 6. Run Freebuff in a sandbox

```sh
./openshell-nixos/freebuff-sandbox                        # Freebuff on ~/Projects
./openshell-nixos/freebuff-sandbox "fix the failing tests" # args pass through
PROJECTS_DIR="$HOME/code" SANDBOX_NAME=fb2 ./openshell-nixos/freebuff-sandbox
DRY_RUN=1 ./openshell-nixos/freebuff-sandbox              # print the openshell command, do nothing
```

The launcher (location-independent — it resolves its own directory for
the policy file) creates a sandbox with exactly the three mounts allowed
on x86_64:

| host | guest | mode | why |
|---|---|---|---|
| `/nix` | `/nix` | ro | every package and shared library, one physical copy (DAX-shared RAM too, via this fork) |
| `/var/lib/fnm` | `/var/lib/fnm` | ro | your one node install (`node`/`npx` for the agent) |
| `$PROJECTS_DIR` | `/workspace` | rw | the code Freebuff edits — your real uid (1000) in the guest |

Because x86_64 allows exactly three mounts, Freebuff's own install does
not get one. Instead the launcher hardlinks the host's
`~/.config/manicode` (step 2's binary, credentials, settings, history)
into the workspace share:

```text
$PROJECTS_DIR/.freebuff-home/.config/manicode   ← same inodes as ~/.config/manicode
```

and runs the guest with `HOME=/workspace/.freebuff-home`, so the native
binary resolves its configDir through the mount. Hardlinks, not a
symlink: they are plain directory entries inside the mounted subtree
(no path outside the share has to resolve through virtiofs), and every
staged file keeps one inode shared with the host. The farm is rebuilt
from the host on every launch — the host dir is the source of truth.

**First run:**

1. the driver pulls `nvcr.io/nvidia/base/ubuntu:24.04` by digest
   (no Docker involved) — plain outbound HTTPS to `nvcr.io` required;
2. the VM boots (10–30 s);
3. OpenShell's policy advisor asks you to **approve Freebuff's network
   egress** (its API endpoints — Freebuff itself is never downloaded
   inside the sandbox; the launcher verified the host binary up front
   and aborts with instructions if it is missing). Answer once; the
   allowlist sticks.

Freebuff's login/history state **is your host's state**: the farm
hardlinks every existing file of `~/.config/manicode`, so writes from
the guest land on the host's real files — a `freebuff` on the host and
one in a sandbox see the same credentials and history.
`openshell sandbox delete` no longer touches freebuff state; remove
`$PROJECTS_DIR/.freebuff-home` for that (it is rebuilt on the next
launch anyway).

## 7. Everyday commands

```sh
openshell sandbox list                                   # what is running
openshell sandbox exec freebuff -- ls /workspace         # run a command inside
openshell sandbox connect freebuff                      # interactive shell
openshell sandbox delete freebuff                       # remove it (and its state)

./mount-check.sh                                        # from openshell-nixos/: verify the three mounts live
journalctl --user -u openshell-gateway -n 50 --no-pager # if you run it as a user unit
```

Useful knobs (all environment variables on the launcher):

| variable | default | meaning |
|---|---|---|
| `PROJECTS_DIR` | `~/Projects` | host directory mounted rw at `/workspace` |
| `SANDBOX_NAME` | `freebuff` | sandbox name (a new name gives you a clean sandbox; Freebuff's own state is the host's — see step 6) |
| `SHARE_NIX_RW` | `0` | `1` mounts `/nix` **writable** — read the security note in [README.md](README.md) first |
| `POLICY_FILE` | `sandbox-policy.yaml` next to the launcher | filesystem policy without which the mounts return `EACCES` |
| `DRY_RUN` | `0` | `1` prints the exact `openshell sandbox create …` instead of running |

## 8. Optional: the NixOS system-level install (this is the sudo path)

Only if you want OpenShell installed **system-wide** on NixOS — a
declarative `/etc/nixos` module (package + KSM service + kvm group +
linger) instead of the user-space flow above. The tutorial's steps 1–7
do **not** depend on it.

```sh
sudo ./openshell-nixos/install.sh           # --dry-run to preview, --help for options
```

After **any** change to the `openshell-nixos/` folder (every commit
counts — the module pins the folder's content hash), relock before your
next rebuild:

```sh
sudo ./openshell-nixos/update-nixos.sh
```

Both commands are relative to the clone root, so they work on any
machine with this fork cloned anywhere. The post-commit hook in
`openshell-nixos/hooks/` (installed at `.git/hooks/post-commit` as a
symlink) prints this reminder automatically when the pin goes stale.

## 9. Troubleshooting

* **`/dev/kvm: Permission denied`** — you are not in the `kvm` group:
  `sudo usermod -aG kvm "$USER"`, log out/in. (The only sudo the main
  path ever needs.)
* **`error: unable to find library -lz3` while building the gateway** —
  re-run the step 3 exports in the shell you build from.
* **`vm-setup: gh is required`** — use the `nix shell nixpkgs#gh
  nixpkgs#zstd -c …` form from step 4.
* **`gateway-vm.sh` asks for mise** — some runtime artifact is missing;
  run the task's script directly: `./tasks/scripts/vm/build-supervisor-bundle.sh`
  (that is exactly what `mise run vm:supervisor` executes).
* **`cargo: command not found`** — you left the toolchain shell; re-enter
  `nix develop` (or your rustup environment).
* **port 18081 already in use** — `OPENSHELL_SERVER_PORT=18082 ./tasks/scripts/gateway-vm.sh`.
* **`no native freebuff binary at ~/.config/manicode/freebuff`** — run
  `freebuff` once on the host (step 2); sandboxes never download it for
  you.
* **`Failed to download freebuff … EACCES`** — must not happen with the
  current launcher (the guest execs the staged host binary directly,
  never the npm wrapper). If you still see it, your `freebuff-sandbox`
  copy predates the workspace-farm mechanism — update it, and do not
  approve the download.
* **Freebuff egress denied (`EACCES` on its endpoints)** — approve the
  endpoints in the policy advisor, or extend `sandbox-policy.yaml`; the
  sandbox is working as intended.
* **mounts return `EACCES`** — the Landlock policy was not passed; the
  launcher does it for you via `--policy "$SCRIPT_DIR/sandbox-policy.yaml"`,
  so a custom invocation must pass one too.
* **first sandbox fails pulling the image** — needs plain HTTPS to
  `nvcr.io` and disk (check `df -h .` and `~/.local/state/openshell`).
* **node dies with `libatomic.so.1: cannot open shared object file`** —
  the launcher already prepends the store's gcc-lib dir to
  `LD_LIBRARY_PATH`; if it exits with "cannot locate the store's
  gcc-lib dir", your host has no nix-ld-style gcc lib in `/nix/store`
  (install nix-ld, or a gcc lib via `nix profile install nixpkgs#gcc`).

## 10. Where things live (all machine-independent)

| what | where |
|---|---|
| gateway config, PKI, dev DB | `.cache/gateway-vm/` (inside the clone) |
| driver state, image cache, overlays | `/tmp/openshell-vm-driver-$USER-vm-dev/` (switches to `~/.local/state` for the system install) |
| registered gateways / active gateway | `~/.config/openshell/` |
| VM runtime artifacts | `target/vm-runtime-compressed/` (inside the clone) |
| freebuff binary + state (credentials, history) | `~/.config/manicode/` on the host, hardlinked into sandboxes via `$PROJECTS_DIR/.freebuff-home` |
| built binaries | `target/debug/` (inside the clone) |

Security decisions (read before weakening): the read-only `/nix`
default, what `SHARE_NIX_RW=1` costs you, why the rw `/workspace` is the
real trust boundary, and the loopback-only gateway — all in
[README.md](README.md).
