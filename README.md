# Niri Session Manager

[![GitHub Actions](https://img.shields.io/endpoint.svg?url=https%3A%2F%2Factions-badge.atrox.dev%2Fnyawox%2Fniri-session-manager%2Fbadge%3Fref%3Dmain&style=for-the-badge&labelColor=11111b)](https://actions-badge.atrox.dev/nyawox/niri-session-manager/goto?ref=main)

A session manager for the Niri Wayland compositor that automatically saves and restores your window layout.

## Features
- Periodic session saving with configurable interval
- Automatic session restoration on startup
- Backup management with configurable retention
- Graceful handling of window spawn failures
- Configurable retry logic for session restoration

## Usage

The program can be run with various command-line options:

```bash
niri-session-manager [OPTIONS]
```

### Options
```
--save-interval <MINUTES>     How often to save the session (default: 15)
--max-backup-count <COUNT>    Number of backup files to keep (default: 5)
--spawn-timeout <SECONDS>     How long to wait for windows to spawn (default: 5)
--retry-attempts <COUNT>      Number of restore attempts (default: 3)
--retry-delay <SECONDS>      Delay between retry attempts (default: 2)
```

## Installation

### Using Nix Flakes

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
            # Optional: Configure the service
            services.niri-session-manager.settings = {
              save-interval = 30;  # Save every 30 minutes
              max-backup-count = 3;  # Keep 3 most recent backups
            };
          })
        ];
      };
    };
  };
}
```

## Limitations

This program assumes the executable:
- Exists in $PATH
- Has the same name as the app ID. In many cases this isn't true, for example: `gamescope`, `cage`.

## Storage

Session data and backups are stored in:
- Session file: `$XDG_DATA_HOME/niri-session-manager/session.json`
- Backups: `$XDG_DATA_HOME/niri-session-manager/session-{timestamp}.bak`

## TODO
- Use PID to fetch the actual process command
