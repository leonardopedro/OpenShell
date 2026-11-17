# SPDX-FileCopyrightText: NVIDIA CORPORATION & AFFILIATES (upstream artifacts, Apache-2.0)
# flake.nix — NVIDIA OpenShell on NixOS.
#
# Packages OpenShell v0.1.5 from the *official* Debian release artifact,
# because:
#   * nixpkgs' `openshell` is CLI-only and pinned at the ancient 0.0.116
#     (the schema-v2 gateway config this install writes would not parse);
#   * OpenShell's own flake exposes devShells/test artifacts, not the
#     shipped binaries;
#   * the upstream install.sh installs .deb/.rpm packages, which does not
#     exist as a mechanism on NixOS.
#
# The deb layout (verified against openshell-checksums-sha256.txt, sha256
# 6af7859a...):
#   usr/bin/openshell                 static (musl)        -> $out/bin
#   usr/bin/openshell-prover          static (musl)        -> $out/bin
#   usr/bin/openshell-gateway         dynamic, glibc-core  -> $out/bin (autoPatchelf)
#   usr/libexec/openshell/openshell-driver-vm
#                                     dynamic, glibc-core  -> $out/bin (autoPatchelf)
# The driver is installed next to the gateway on purpose: the gateway
# resolves `openshell-driver-vm` from driver_dir, the conventional
# libexec locations, then *a sibling of the gateway binary* — so
# $out/bin/openshell-driver-vm is found without any config key.
#
# This directory is independent of ../unfer and ../australVM; the shared
# /nix-store mechanism they pioneered is credited in README.md.
#
# Consumption on this machine: install.sh copies openshell-package.nix
# into /etc/nixos and the rendered openshell.nix module callPackages it —
# this flake and the system build share ONE derivation file, with no
# flake-ref / narHash pin between them (no nix profile, no global
# registry, nothing to re-sync when this folder changes). The derivation
# itself lives in openshell-package.nix, used by both consumers.
{
  description = "NVIDIA OpenShell (libkrun microVM agent sandbox) packaged for NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system} = {
        # The derivation is factored into openshell-package.nix so the
        # /etc/nixos module can consume the same file without a flake ref
        # (one source; no second copy to drift).
        openshell = pkgs.callPackage ./openshell-package.nix { };

        # herdr-core (`hc`) from the pinned official release tarball (the
        # pinned nixpkgs predates it). Herdr TUI comes from nixpkgs: it is the
        # same 0.9.5 source as ../herdr, without pinning that checkout here.
        herdr-core = pkgs.callPackage ./herdr-core-package.nix { };
        herdr = pkgs.herdr;

        # Everything needed to drive the fleet from a project folder. Freebuff
        # and the Infinity --release binaries stay external: Freebuff's native
        # binary/credentials live in ~/.config/manicode, and Infinity is baked
        # into the shared store by infinity-sandbox at launch time.
        fleet = pkgs.callPackage ./fleet-package.nix {
          openshell = self.packages.${system}.openshell;
          herdr-core = self.packages.${system}.herdr-core;
          herdr = self.packages.${system}.herdr;
        };

        default = self.packages.${system}.fleet;
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${self.packages.${system}.openshell}/bin/openshell";
        };
        hc = {
          type = "app";
          program = "${self.packages.${system}.herdr-core}/bin/hc";
        };
        freebuff-here = {
          type = "app";
          program = "${self.packages.${system}.fleet}/bin/freebuff-here";
        };
      };
    };
}
