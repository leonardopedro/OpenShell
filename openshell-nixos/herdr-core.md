# herdr-core: control the sandboxed agents from another device

`herdr-core` (`hc`) is the headless runtime behind the Herdr desktop and
mobile apps. It runs programs in persistent terminals that survive
disconnects, and exposes them over a local API that the phone app reaches over
SSH. This folder packages it with Nix and connects it to the Infinity-in-
OpenShell stack documented in [infinity-sandbox.md](infinity-sandbox.md).

```text
phone / other device                    this host
┌──────────────────────┐                ┌──────────────────────────────────────┐
│ Herdr Mobile app     │── SSH ────────▶│ hc (herdr-core, systemd --user)      │
│ (or `hc` on a laptop)│   Tailscale    │  └ terminal: herdr-core-agent        │
└──────────────────────┘   optional     │      └ infinity-sandbox              │
                                        │          └ OpenShell cloud-hypervisor│
                                        │              VM: Infinity harness    │
                                        │  Edge0 35B served on the host (:8000 │
                                        │  CPU / :8001 Vulkan)                 │
                                        └──────────────────────────────────────┘
```

The agent runs **inside** OpenShell; `hc` only owns the terminal. Neither the
core nor the phone gets access to files or network beyond what OpenShell's
policy allows.

## Install with Nix

The flake exposes `herdr-core` (pinned official static release, currently
0.1.21) and `herdr` (the TUI from the pinned nixpkgs, 0.9.5 — same source as
`../../herdr`):

```sh
# one-off
nix run path:$(pwd)#hc -- --version

# permanent (user profile, no root)
nix profile install path:$(pwd)#herdr-core
nix profile install path:$(pwd)#herdr      # optional desktop TUI

# or just build and use the store path
nix build path:$(pwd)#herdr-core --print-out-paths
```

`hc` is a symlink to `herdr-core` in the package's `bin/`. The Linux build is
static, so no `patchelf` and no runtime library setup is needed. The separate
`../../herdr-core` source clone has no flake or Nix package of its own (it
needs Zig 0.16 and the vendored ghostty tree to build from source), so this
derivation pins the official published binary instead; the clone remains
useful as a source reference. Updating is
`nix profile upgrade <index-or-name>` or bumping the pinned hash/version in
[herdr-core-package.nix](herdr-core-package.nix) and rebuilding.

## Configure and start the core

```sh
install -Dm600 herdr-core-config.toml ~/.config/herdr-core/config.toml
hc server start
hc server status          # prints the live core's version and PID
```

`notes = false` in the template matters: the Infinity TUI owns its screen,
and parent-note text injected by `hc` would corrupt its rendering. The core
keeps its own environment when restarted; use `hc server restart --fresh`
from a clean shell if PATH/HOME must be re-read.

## Launch an agent as a persistent terminal

```sh
# CPU-served Edge0 on :8000, interactive Infinity TUI
./herdr-core-agent edge0

# one-shot task, no TUI
./herdr-core-agent edge0 -- --headless "review the failing test"

# host GPU model on :8001
SANDBOX_GPU=1 ./herdr-core-agent edge0-gpu

# inspect / take over / stop
./herdr-core-agent --list
./herdr-core-agent --attach edge0        # Ctrl-\ detaches
./herdr-core-agent --read edge0
./herdr-core-agent --stop edge0
```

The launcher starts the core if needed, then runs `infinity-sandbox` inside
an `hc` terminal with labels `app=infinity` and `runtime=openshell`. A bare
task becomes Infinity's `--initial-message`; other flags pass through (see
[infinity-sandbox.md](infinity-sandbox.md#passing-a-task)). It does **not**
put the Edge0 API key in the saved argv — the sandbox launcher reads
`~/.edge0/api-key` itself.

`DRY_RUN=1 ./herdr-core-agent edge0` prints the exact `hc run` command.
`HC_BIN`, `INFINITY_SANDBOX` and `PROJECTS_DIR` override the resolved paths.

## Connect the phone

1. Install **Herdr Mobile** on the phone (Android/iOS). It talks directly to
   `hc` over SSH; there is no Herdr account or relay in between.
2. Make sure this host accepts SSH: `sudo systemctl enable --now ssh` and
   open port 22 (or your custom port) on the host firewall. Add your phone's
   public key to `~/.ssh/authorized_keys`.
3. For access away from home, install [Tailscale](https://tailscale.com/) on
   both devices and give the phone the host's Tailscale address. Tailscale
   SSH also works without managing keys.
4. In the app, add a machine with the host address, user and port. The app
   lists the terminals created by `herdr-core-agent`, lets you read/answer
   them, and opens a real terminal for direct input.

If the app says Herdr is missing, it is usually a PATH problem in a
non-interactive SSH session: make sure `hc` is visible from a login shell
(`nix profile install` puts it in `~/.nix-profile/bin`; add that to your
shell profile if needed). `hc machine status` on the host is a useful
non-interactive connectivity check.

## Several machines

On every agent host, install the same flake packages and run
`herdr-core-agent`. From one machine, register the others:

```sh
hc machine add user@box2 --name box2
hc run --on box2 --bg --name other-agent -- /path/to/herdr-core-agent
hc attach box2/t7                  # use the address the run prints
hc machine ls
```

Programs and files stay on the machine where they run; only the terminal
stream crosses SSH. Copy the launcher and the Nix store closure with
`nix-copy-closure --to user@box2 /nix/store/…-herdr-core-0.1.21`, or deploy
the same flake on each host. A machine can also reach other machines already
in its SSH config, so the phone discovers them from the first host.

## Verification status

Checked here:

- `nix build path:…#herdr-core` and `#herdr` succeed; `nix profile install
  path:…#herdr-core` puts `hc 0.1.21` on `PATH`, with the `bin/hc` symlink.
- Config installed to `~/.config/herdr-core/config.toml`; the core was
  restarted with the 0.1.21 binary and `hc server status` reports
  `version: 0.1.21`, `installed_by: nix`, `keeps_running_after_logout: true`.
- `bash -n herdr-core-agent`; `DRY_RUN=1` produces the expected `hc run --bg`
  command with the Infinity sandbox launcher and `--headless` passthrough.
- Real `hc run --bg` smoke test: a terminal owned by the core starts, and the
  wrapper's OSC 7501 report is recorded by the core as
  `status: {app: infinity, state: idle}`. The smoke terminal was removed.

Not checked here (needs the actual devices/gateway):

- Herdr Mobile app connecting over SSH, host-key approval, Tailscale.
- A real `hc run` starting the Infinity sandbox and a full VM boot — the
  Node/Edge0 model server, gateway VM driver and app are not part of this
  packaging change. `sshd` is not enabled on this host yet; enable it before
  expecting the phone to connect.

## Limitations

- **Status is coarse.** herdr-core gets agent state from OSC 7501 reports.
  Infinity does not emit them yet, so the wrapper reports `idle` while the
  agent is up and `done` when it exits. Per-turn `working`/`blocked` would
  need an OSC 7501 integration in Infinity itself.
- **One terminal per agent.** Each `herdr-core-agent` name is one sandbox;
  parallel threads inside Infinity still share that VM. Use several named
  terminals for independent agents.
- **The phone app is not a TUI.** It shows terminals and agents from
  herdr-core, not panes from the `herdr` TUI; both can run on the same host.
- **SSH is the only transport.** No credentials or agent data go through a
  Herdr server; host-key checking and SSH keys/Tailscale are the security
  boundary, so protect them accordingly.
