# Niri Session Manager

[![GitHub Actions](https://img.shields.io/endpoint.svg?url=https%3A%2F%2Factions-badge.atrox.dev%2Fnyawox%2Fniri-session-manager%2Fbadge%3Fref%3Dmain&style=for-the-badge&labelColor=11111b)](https://actions-badge.atrox.dev/nyawox/niri-session-manager/goto?ref=main)

i don't know what i'm doing. help me :crying_cat:

This program assumes the executable:
- Exists in $PATH
- Has the same name as the app ID. In many cases this isn't true, for example: `gamescope`, `cage`.
I've made it just to save few keystrokes every time I launch qemu (and restart the compositor to detach the GPU).

```nix
{
  description = "Your NixOS configuration";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    niri-session-manager.url = "github:nyawox/niri-session-manager";
  };
  outputs = { self, nixpkgs, niri-session-manager, ... }: {
    nixosConfigurations = {
      yourHost = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          # This is not a complete NixOS configuration; reference your normal configuration here.
          # Import the module
          niri-session-manager.nixosModules.niri-session-manager

          ({
            # Enable the service
            services.niri-session-manager.enable = true;
          })
        ];
      };
    };
  };
}
```

## TODO
- Save the session periodically
- Use PID to fetch the actual process command
