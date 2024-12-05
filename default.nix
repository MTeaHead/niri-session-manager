{
  makeRustPlatform,
  fenix,
  system,
  name,
  version,
}:
let
  rustPlatform = makeRustPlatform {
    cargo = fenix.packages.${system}.minimal.toolchain;
    rustc = fenix.packages.${system}.minimal.toolchain;
  };
in
rustPlatform.buildRustPackage {
  pname = name;
  inherit version;
  src = ./.;
  cargoLock = {
    lockFile = ./Cargo.lock;
    allowBuiltinFetchGit = true;
  };
  meta = {
    mainProgram = "niri-session-manager";
  };
}
