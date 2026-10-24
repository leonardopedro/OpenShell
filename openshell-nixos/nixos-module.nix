# managed-by: openshell-nixos/install.sh — rewritten from that folder's
# nixos-module.nix template whenever the rendered result differs. Delete
# this marker line if you start editing this file by hand; the installer
# then treats it as hand-managed and leaves it alone.
#
# Everything OpenShell needs at the system level, declared the NixOS way.
# The imperative FHS recipe cannot work on NixOS: /etc/systemd/system is a
# symlink into the immutable Nix store, so `tee`ing a unit file there fails
# by design — which is exactly how the first attempt at this install died.
# One module + one `nixos-rebuild switch` replaces: a `nix profile install`
# (package), a hand-written unit (KSM), a `usermod` (kvm group) and a
# `loginctl enable-linger` (linger).
{ config, pkgs, lib, ... }:

{
  # ---------------------------------------------------------------------------
  # 1 — Package: the CLI, gateway and libkrun VM driver on every user's PATH
  # via the system profile (/run/current-system/sw/bin), which is also first
  # in the gateway user unit's PATH. The installer substitutes a *locked*
  # ref (path + narHash of this machine's openshell-nixos folder), so eval
  # pulls the official v0.1.5 .deb build from the store by content hash —
  # no re-download, no rebuild, and immune to the folder changing under us
  # (rerunning install.sh re-syncs the hash when the folder does change).
  environment.systemPackages = [
    (builtins.getFlake "@OPENSHELL_REF@").packages.${pkgs.stdenv.hostPlatform.system}.openshell
  ];

  # ---------------------------------------------------------------------------
  # 2 — KSM: ksmd runs so any madvise(MADV_MERGEABLE) memory on this
  # machine gets merged (the guest RAM here never registers — see below).
  # The RAM sharing microVMs actually rely on is the host page cache
  # behind virtiofs. Same unit the imperative recipe wrote, now declared;
  # install.sh substitutes the KSM flag with literal
  # true or false (--no-ksm writes false, leaving the block inert).
  # BEGIN ksm-sharing
  systemd.services.ksm-sharing = lib.mkIf @KSM_ENABLED@ {
    description = "KSM same-page merging for OpenShell microVM memory sharing";
    documentation = [ "man:ksmd(8)" ];
    wantedBy = [ "multi-user.target" ];
    after = [ "sysinit.target" ];
    before = [ "basic.target" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Store-absolute shell (there is no reliable /bin/sh outside the store).
      # run=1 ("madvise"), NOT run=2 — measured and documented on this
      # machine (kernel 7.2.9, docs v7.3 "KSM daemon sysfs interface"):
      # run=2 is *not* an always-merge mode, it means "stop ksmd and
      # unmerge all pages currently merged". Writing it parks ksmd cold
      # (observed: 0 CPU over 9 s, full_scans frozen at 0) and the
      # run change zeroes the scan counters. run=1 keeps ksmd active;
      # it scans only memory registered with madvise(MADV_MERGEABLE), and
      # libkrun never madvises its guest RAM (QEMU only does with explicit
      # mem-merge; libkrun has no such option), so KSM sees no guest
      # candidates. File-backed sharing is handled elsewhere: the fork
      # driver's virtiofs DAX window (Route A) gives host<->guest one
      # physical copy of every shared file — ksmd is not what provides it
      # (measured — README's "KSM reality check").
      # use_zero_pages still collapses any madvised zero pages onto the
      # shared zero page when candidates do appear.
      ExecStart = "${pkgs.runtimeShell} -c 'echo 1 > /sys/kernel/mm/ksm/run; echo 1 > /sys/kernel/mm/ksm/use_zero_pages'";
      ExecStop = "${pkgs.runtimeShell} -c 'echo 0 > /sys/kernel/mm/ksm/run'";
    };
  };
  # END ksm-sharing

  # ---------------------------------------------------------------------------
  # 3 — kvm group for the sandbox user (the driver needs /dev/kvm). The
  # installer deletes the line below when the system has no kvm group.
  users.users."@USER@".extraGroups = [ "kvm" ];

  # ---------------------------------------------------------------------------
  # 4 — linger: the user's gateway service starts at boot instead of waiting
  # for a login session (the upstream docs' own recommendation, now
  # declarative instead of a one-shot `loginctl enable-linger`).
  users.users."@USER@".linger = true;
}
