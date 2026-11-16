# herdr-core-package.nix — herdr-core (`hc`), the headless runtime behind the
# Herdr desktop/mobile apps.
#
# The OpenShell flake pins a nixpkgs revision that predates herdr-core, and
# building it from source needs Zig 0.16 + the vendored ghostty tree. The
# official release builds are static (see herdr-core's README), so packaging
# the published tarball is both smaller and more reproducible: one fetchurl
# with a pinned SHA-256 per target, no toolchain and no patchelf.
#
# Hashes come from https://releases.herdr.dev/core/0.1.21/manifest.json.
{ lib, stdenv, fetchurl }:

let
  version = "0.1.21";
  targets = {
    "x86_64-linux" = {
      url = "https://releases.herdr.dev/core/${version}/herdr-core-linux-x86_64.tar.gz";
      sha256 = "78424755c10a40dde38a0bdb55c20ac19706e9ae05ed68fc2acf1fcff0d941c1";
    };
    "aarch64-linux" = {
      url = "https://releases.herdr.dev/core/${version}/herdr-core-linux-aarch64.tar.gz";
      sha256 = "e844a2bfab48a226097679d076bc83042fe57fccef5387ef802e5b24e78b529a";
    };
    "x86_64-darwin" = {
      url = "https://releases.herdr.dev/core/${version}/herdr-core-macos-x86_64.tar.gz";
      sha256 = "e377e1d342cddce0346da5b2f3c281b9701e4731c00024c7c5028265e77a217f";
    };
    "aarch64-darwin" = {
      url = "https://releases.herdr.dev/core/${version}/herdr-core-macos-aarch64.tar.gz";
      sha256 = "1ab38dd9270f63a8f794d6d60e8702eb0d1c85a72bda85d735e5eea43f05ffed";
    };
  };
  target = targets.${stdenv.hostPlatform.system}
    or (throw "herdr-core: no release artifact for ${stdenv.hostPlatform.system}");
in
stdenv.mkDerivation {
  pname = "herdr-core";
  inherit version;

  src = fetchurl target;

  # The tarball contains a bare `herdr-core` file, not a directory, so the
  # default unpackPhase (which insists on a top-level dir) cannot be used.
  sourceRoot = "source";
  unpackPhase = ''
    runHook preUnpack
    mkdir -p source
    tar -xzf "$src" -C source
    runHook postUnpack
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 herdr-core "$out/bin/herdr-core"
    ln -s herdr-core "$out/bin/hc"
    runHook postInstall
  '';

  # Static binary (Linux) / App bundle payload (macOS): no ELF patching,
  # no interpreter rewriting, no stripping of Nix debug metadata.
  dontStrip = true;

  meta = {
    description = "herdr-core: programs and coding agents in terminals that stay alive";
    homepage = "https://github.com/herdrdev/herdr-core";
    license = lib.licenses.asl20;
    platforms = builtins.attrNames targets;
    mainProgram = "hc";
  };
}
