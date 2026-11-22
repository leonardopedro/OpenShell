# Fleet plan: 8 Freebuff Starter sessions + 3 local LLMs, one VM per laptop, Android control

## Goal

- **8 Freebuff sessions** (paid **Starter** plan, ~7–8 €/mo) spread over **3+ laptops**.
- **3 local LLMs** (Edge0 35B on each laptop) serving Infinity Code agents.
- Everything inside **OpenShell** microVMs with **Nix store memory sharing** (libkrun virtiofs + DAX window).
- One control point: **herdr-core (`hc`)** aggregated over **Tailscale SSH**, driven from the **Herdr Mobile** Android app.
- **One VM per laptop** — multiple sessions share that VM; 3 VMs total, not 11.

## Research conclusions (2026-11-16)

- Freebuff ads remain on **all** plans, including Starter — no ad-free tier is advertised. Ads are shown between turns and impressions are required before the next turn. Do **not** strip ANSI, use pipes, or run headless wrappers.
- The Freebuff CLI has **no headless path** even on paid plans: no `-p/--print`, `--json`, prompt argument; `@codebuff/sdk` needs a separate paid `CODEBUFF_API_KEY` (`cb_...`) from Codebuff, not the Freebuff Starter login. Third-party `freebuff2api`-style proxies violate ToS and risk bans.
- Starter adds **50 h/day + 30 h monthly bonus**, all models, unlimited messages per session — enough for 8 sessions on one account; no account rotation needed initially.
- Tailscale SSH removes the need for `sshd`, SSH keys and host-key distribution. Herdr Mobile explicitly supports Tailscale SSH.
- OpenShell VM driver: bind mounts and DAX windows are a **libkrun** feature. Use `OPENSHELL_VM_DAX_WINDOW_MIB=128` on each gateway; do not use the cloud-hypervisor backend.

## Architecture

```text
Android: Herdr Mobile ──SSH (Tailscale)──▶ control laptop: hc
                                             ├─ fb-1..3, inf-1  → VM on laptop 1
                                             ├─ fb-4..6, inf-2  → VM on laptop 2
                                             └─ fb-7..8, inf-3  → VM on laptop 3
                                           (hc machine add links laptop 2/3)

Per laptop:  1 OpenShell VM (libkrun + DAX: /nix ro, /workspace rw)
             ├─ 2–3 Freebuff TUIs (ads render, separate HOME + state each)
             ├─ 1 Infinity Code harness → host.openshell.internal:8001
             └─ host: Edge0 35B server on :8001 (Vulkan, API key file)
```

## Per-laptop setup (once)

1. OpenShell + Nix installed; gateway with VM driver; `virtiofsd` on PATH; `OPENSHELL_VM_DAX_WINDOW_MIB=128` in the gateway environment.
2. `nix profile install path:…/OpenShell/openshell-nixos#herdr-core`; `hc server start`.
3. `sudo tailscale set --ssh`; Tailnet ACL permits SSH (`"action": "ssh", "src": [...], "dst": [...]`). No `openssh-server` needed.
4. Freebuff Starter login once on the host so `~/.config/manicode/{freebuff,credentials.json}` exists; sandboxes never download.
5. Edge0 server on the host (one per laptop), e.g.:
   ```sh
   GGML_VK_VISIBLE_DEVICES=1 edge0-infer serve --model "$MOE_ASSETS_35B" \
     --device vulkan --model-option expert_slots=24 \
     --host 0.0.0.0 --port 8001 --api-key-file ~/.edge0/api-key
   ```
   Import `providers/edge0-openai-gpu.yaml` and create provider `edge0-gpu`.

## Install as one Nix package

From this directory:

```sh
nix profile install path:$PWD#fleet
```

The `fleet` package installs the launchers (`freebuff-here`, `fb-fleet`,
`infinity-sandbox`, `herdr-core-agent`, `freebuff-sandbox`), the sandbox/provider
policies and herdr-core config beside them, plus the docs in
`share/doc/agent-fleet`. It also brings OpenShell, herdr-core and the Herdr TUI
onto `PATH` (the launchers are wrapped with those paths, so `nix run` works too):

```sh
nix run path:$PWD#freebuff-here -- --help
```

Freebuff and the Infinity `--release` binaries stay external by design: Freebuff's
native binary and live credentials live in `~/.config/manicode` and are staged
per session; Infinity is baked into the shared Nix store at launch time by
`infinity-sandbox`. A pure flake cannot reference a local build tree outside the
repo, which is why they are not store derivations here.

## Per-project launcher: `freebuff-here`

From any project folder, run `fb` (short alias) or this repo's `openshell-nixos/freebuff-here`:

```sh
cd ~/my-project
../OpenShell/openshell-nixos/freebuff-here                 # session named after the folder
../OpenShell/openshell-nixos/freebuff-here "fix the test"  # also sends the first task
DRY_RUN=1 ../OpenShell/openshell-nixos/freebuff-here       # print commands only
```

It copies the current directory into the fleet VM's `/workspace/<name>/project`,
stages a per-session Freebuff state dir, adds the five egress endpoints for that
session's binary, and starts an `hc` terminal named `<folder>-fb`. The terminal
appears in Herdr Mobile automatically. `FLEET_DIR` and `SANDBOX_NAME` override
the shared workspace and VM. A new session needs one device login (codes expire
in ~10 minutes); existing sessions keep their staged login.

## One VM, many sessions

A sandbox is a VM with an exec channel. Keep one VM alive per laptop and open one `hc` terminal per session into it:

```sh
hc run --bg --name fb-1 -- openshell sandbox exec --tty fleet -- \
  env HOME=/workspace/fb-1 /workspace/fb-1/.config/manicode/freebuff
hc run --bg --name inf-1 -- openshell sandbox exec --tty fleet -- \
  env HOME=/workspace/inf-1 /nix/store/…-infinity/bin/infinity
```

State is staged on the host into `$FLEET_DIR/<session>/.config/manicode` before VM start with `cp -a` (not hardlinks — concurrent sessions would otherwise share writes). Infinity's `providers.json` points at `http://host.openshell.internal:8001/v1`; the API key reaches the guest via `--env-from INFINITY_OPENAI_API_KEY` on the VM.

Mounts: `/nix` ro and `$FLEET_DIR` → `/workspace` rw. Both Freebuff and Infinity fit in two mounts (the three-mount limit is not hit).

## Aggregation and Android

- On the control laptop: `hc machine add leo@100.x.y.z --name l2` (and l3), using Tailscale IPs.
- Herdr Mobile: add the control laptop's Tailscale IP, user, port 22. The app shows all terminals in one list with real ANSI (including Freebuff ad boxes).
- The app supports multiple profiles but one active SSH session at a time — hence the single aggregated `hc`.

## Quota / model policy

- Start with 3 sessions, scale to 8; watch Starter's fair-use limits.
- Bulk work → unlimited models (Solar Pro 4, Solar Mini 4, Beam 1.0, Nemotron 3.5 Lightning, Glyph Cluster).
- Hard tasks → DeepSeek V4.1 Flash, GPT-6 Luna, Claude Haiku 5.5.
- Overflow → the 3 local Edge0 agents.

## Risks

| Risk | Mitigation |
|---|---|
| Ads required | Keep the TUI in a PTY; never strip ANSI or use headless/proxy wrappers |
| Starter is not an API plan | Don't assume SDK/headless access |
| 8 sessions on one account may hit fair-use | Stagger starts; cheap models for bulk; local LLM overflow |
| One VM = no isolation between sessions | Accept, or split into 2 VMs/laptop if needed |
| Egress denied by OpenShell policy | Approve Freebuff endpoints once per VM via policy advisor |
| Phone one active SSH session | Aggregate via control laptop's `hc` |

## Execution status (2026-11-16)

- [x] Plan written (`fleet-plan.md`).
- [x] `fb-fleet` launcher implemented with a write-free `--plan` default and explicit `--apply`: stages the shared Freebuff binary and Infinity release binaries, creates one libkrun+DAX VM with `/nix` ro + project rw, verifies the returned VM ID/Ready/provider/policy/mounts before starting any `hc` terminal, and reconciles retries by terminal name. Credentials are never copied; each new session stays a separate operator login step. Stub acceptance: `tests/fb-fleet-test.sh` (plan writes nothing, repeat apply reuses the VM, stale policy/wrong VM/failed create/not-Ready start no terminal, and an interrupted launch is reconciled without duplication).
- [x] Laptop-1 slice executed here: VM `fleet` is Ready; 3 Freebuff binaries + 1 Infinity harness (with its provider) are running inside it; `hc ls` shows `fb-1`, `fb-2`, `fb-3`, `inf-1`.
- [x] Freebuff egress approved through `fleet-policy.yaml`: `freebuff.com:443`, `www.freebuff.com:443`, `www.codebuff.com:443`, `codebuff.com:443`, `api.codebuff.com:443` for the three Freebuff binaries. The critical missing host was **`www.codebuff.com`** — its denial (`policy_dns_ineligible` / `transparent_tcp_policy_denied`) is what produced "Can't reach Freebuff's servers". Diagnose with `openshell logs <VM> --since 5m --source sandbox`.
- [x] Verified a real Freebuff turn inside the VM: after login it auto-switched to the unlimited `Beam 1.0` model (DeepSeek Flash Fast needed 3 h of allowance) and replied `PONG`.
- [x] Verified the Infinity terminal inside the VM reaches Edge0 through `host.openshell.internal:8001` and answers `PONG.`
- [ ] **User action:** `fb-2` and `fb-3` still need one interactive device login in the VM — the host credentials are keyring-backed, so `~/.config/manicode/credentials.json` alone does not authenticate in the guest. Press Enter on their screens to get fresh links and approve them promptly (codes expire in ~10 min).
- [ ] Laptops 2 and 3: identical `fb-fleet` run by the operator (only one machine is available in this session).
- [ ] `hc machine add` for l2/l3 and the Android Herdr Mobile connection (needs the other machines/phone).
- [ ] Tailscale SSH: `sudo tailscale set --ssh` plus an SSH ACL on the tailnet (no sudo was available in this agent session; no sudo was needed for the executed steps).
