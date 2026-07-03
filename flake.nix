{

  description = "niri-session-manager";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    systems.url = "github:nix-systems/default";
    treefmt-nix.url = "github:numtide/treefmt-nix";
  };

  outputs =
    {
      nixpkgs,
      systems,
      treefmt-nix,
      self,
    }:
    let
      forAllSystems =
        function: nixpkgs.lib.genAttrs (import systems) (system: function nixpkgs.legacyPackages.${system});
      treefmtEval = forAllSystems (
        pkgs:
        treefmt-nix.lib.evalModule pkgs (
          { pkgs, ... }:
          {
            programs = {
              nixfmt-rfc-style.enable = true;
              statix.enable = true;
            };
            projectRootFile = "flake.nix";
          }
        )
      );
      getPlatform = p: p.hostPlatform.system;
    in
    {
      formatter = forAllSystems (pkgs: treefmtEval.${getPlatform pkgs}.config.build.wrapper);

      checks = forAllSystems (pkgs: {
        formatting = treefmtEval.${getPlatform pkgs}.config.build.check self;
      });

      nixosModules = {
        niri-session-manager =
          { pkgs, ... }:
          {
            imports = [
              ./module.nix
            ];
            services.niri-session-manager.package = self.packages.${getPlatform pkgs}.niri-session-manager;
          };
      };

      packages = forAllSystems (pkgs: {
        default = self.packages.${getPlatform pkgs}.niri-session-manager;
        niri-session-manager = pkgs.rustPlatform.callPackage ./default.nix { };
      });

      devShells = forAllSystems (pkgs: {
        default = import ./shell.nix { inherit pkgs; };
      });

      overlays.niri-session-manager = final: prev: {
        inherit (self.packages.${prev.hostPlatform.system}) niri-session-manager;
      };
    };
}
