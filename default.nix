{ buildRustPackage, lib }:
buildRustPackage (
  finalAttrs:
  let
    cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  in
  {
    pname = "niri-session-manager";
    src = lib.sources.cleanSourceWith {
      src = ./.;
      filter =
        path: type:
        type == "directory"
        || lib.any (ext: lib.hasSuffix ext (baseNameOf path)) [
          ".rs"
          ".toml"
          ".lock"
          ".nix"
        ];
    };
    inherit (cargoToml.package) version;

    cargoLock = {
      lockFile = ./Cargo.lock;
    };

    meta.mainProgram = "niri-session-manager";
  }
)
