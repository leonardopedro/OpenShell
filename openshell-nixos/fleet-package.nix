# One package that installs the whole agent-fleet control layer: launchers,
# sandbox/provider policies, herdr-core config and the deployment docs, plus
# the OpenShell, Herdr and Infinity packages on PATH.
#
# Freebuff itself is intentionally NOT packaged: its native binary and live
# credentials stay in ~/.config/manicode, which the launchers stage per session.
{ stdenv, lib, makeWrapper, openshell, herdr-core, herdr }:

let
  scripts = [
    "infinity-sandbox"
    "herdr-core-agent"
    "fb-fleet"
    "freebuff-here"
    "fb"
    "freebuff-sandbox"
  ];
  configs = [
    ./sandbox-policy.yaml
    ./fleet-policy.yaml
    ./herdr-core-config.toml
    ./gateway.toml
    ../providers/edge0-openai.yaml
    ../providers/edge0-openai-gpu.yaml
  ];
  docs = [
    "README.md"
    "infinity-sandbox.md"
    "herdr-core.md"
    "fleet-plan.md"
  ];
  # Path literals are copied into the store with a hash prefix; recover the
  # original file name for the installed config files.
  fileName = f:
    let m = builtins.match "[a-z0-9]{32}-(.*)" (builtins.baseNameOf f);
    in if m == null then builtins.baseNameOf f else builtins.head m;
in
stdenv.mkDerivation {
  pname = "agent-fleet";
  version = "0.1.0";
  src = ./.;
  dontBuild = true;
  dontConfigure = true;

  nativeBuildInputs = [ makeWrapper ];
  propagatedBuildInputs = [ openshell herdr-core herdr ];

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/bin" "$out/share/doc/agent-fleet" "$out/share/agent-fleet/providers"

    ${lib.concatMapStringsSep "\n" (f: ''
      install -Dm755 ${./.}/${f} "$out/bin/$(basename ${f})"
    '') scripts}

    ${lib.concatMapStringsSep "\n" (f: ''
      install -Dm644 ${f} "$out/bin/${fileName f}"
    '') configs}

    ${lib.concatMapStringsSep "\n" (f: ''
      install -Dm644 ${./.}/${f} "$out/share/doc/agent-fleet/$(basename ${f})"
    '') docs}

    patchShebangs "$out/bin"
    runHook postInstall
  '';

  postFixup = ''
    # `nix run` does not necessarily put propagated inputs on PATH, so wrap
    # every executable launcher with the OpenShell/Herdr binaries explicitly.
    for f in "$out"/bin/*; do
      if [[ -x "$f" && ! -L "$f" ]]; then
        wrapProgram "$f" --prefix PATH : ${lib.makeBinPath [ openshell herdr-core herdr ]}
      fi
    done
  '';

  meta = {
    description = "Freebuff + Infinity + OpenShell + herdr-core agent fleet tooling";
    license = lib.licenses.asl20;
    platforms = lib.platforms.linux;
    mainProgram = "freebuff-here";
  };
}
