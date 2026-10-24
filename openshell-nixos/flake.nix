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
# Consumption on this machine: /etc/nixos/openshell.nix (rendered by
# install.sh) holds a locked `builtins.getFlake` ref to this folder, so
# `nixos-rebuild switch` builds packages.x86_64-linux.openshell straight
# from here — no nix profile, no global registry.
{
  description = "NVIDIA OpenShell (libkrun microVM agent sandbox) packaged for NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      version = "0.1.5";
    in
    {
      packages.${system} = {
        openshell = pkgs.stdenv.mkDerivation {
          pname = "openshell";
          inherit version;

          src = pkgs.fetchurl {
            url = "https://github.com/NVIDIA/OpenShell/releases/download/v${version}/openshell_${version}-1_amd64.deb";
            hash = "sha256-aveFmoerCzv5au8iFtsZG1lzCKwA35v+XQ5emRL44qs=";
          };

          nativeBuildInputs = [
            pkgs.autoPatchelfHook
            pkgs.binutils # ar
            pkgs.zstd # tar --zstd
          ];

          # Both dynamic binaries need only glibc core (librt, libpthread,
          # libm, libdl, libc) — provided by stdenv. libkrun/libkrunfw and
          # the guest runtime are embedded in the driver, not linked.
          buildInputs = [ ];

          unpackPhase = ''
            runHook preUnpack
            ar x "$src"
            tar --zstd -xf data.tar.zst
            runHook postUnpack
          '';

          # After unpacking, the only directory created is usr/ (the deb root).
          sourceRoot = "usr";

          installPhase = ''
            runHook preInstall

            install -Dm755 bin/openshell             "$out/bin/openshell"
            install -Dm755 bin/openshell-gateway      "$out/bin/openshell-gateway"
            install -Dm755 bin/openshell-prover       "$out/bin/openshell-prover"
            # Sibling of the gateway binary = the gateway's own last-resort
            # driver resolution path; no driver_dir config needed.
            install -Dm755 libexec/openshell/openshell-driver-vm \
                                   "$out/bin/openshell-driver-vm"

            # Keep the upstream user unit, with Nix store paths substituted,
            # as a reference copy (install.sh writes the live unit into
            # ~/.config/systemd/user/ with the same content).
            install -Dm644 lib/systemd/user/openshell-gateway.service \
                   "$out/lib/systemd/user/openshell-gateway.service"
            substituteInPlace "$out/lib/systemd/user/openshell-gateway.service" \
              --replace-fail '/usr/bin/openshell-gateway' "$out/bin/openshell-gateway"

            runHook postInstall
          '';

          # Strips the RPATHs the deb was built with; static binaries are
          # left alone (no PT_INTERP to rewrite).
          autoPatchelfIgnoreMissingDeps = false;

          meta = {
            description = "Safe, private runtime for autonomous AI agents (NVIDIA OpenShell)";
            homepage = "https://docs.nvidia.com/openshell";
            license = nixpkgs.lib.licenses.asl20;
            platforms = [ system ];
            mainProgram = "openshell";
          };
        };

        default = self.packages.${system}.openshell;
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${self.packages.${system}.openshell}/bin/openshell";
        };
      };
    };
}
