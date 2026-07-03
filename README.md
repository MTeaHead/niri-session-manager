# Niri Session Manager

A session manager for the Niri Wayland compositor that automatically saves and restores your window layout.

## Features
- Periodic session saving with configurable interval
- Automatic session restoration on startup
- Backup management with configurable retention
- Graceful handling of window spawn failures
- Configurable retry logic for session restoration
- Custom app launch command mapping via TOML configuration
- **Terminal state recovery** — restores running commands inside terminals (e.g. `btop`, `nvim`, `ssh`)

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
--retry-delay <SECONDS>       Delay between retry attempts (default: 2)
```

## Configuration

The program supports mapping app IDs to custom launch commands via a TOML configuration file. This is useful for applications where the app ID doesn't match the executable name, or when special launch arguments are needed.

Configuration file location: `$XDG_CONFIG_HOME/niri-session-manager/config.toml`

Example configuration:
```toml
# Niri Session Manager Configuration

# Apps that should only have one instance
[single_instance_apps] 
apps = [
    "firefox",
    "zen"
]

# Applications to skip during startup
[skip_apps]
apps = [
    "discord",
    "slack"
]

#Application remapping
[app_mappings]

# flatpak remapping
"vesktop" = ["flatpak", "run", "dev.vencord.Vesktop"]
"discord" = ["flatpak", "run", "com.discordapp.Discord"]
"slack" = ["flatpak", "run", "com.slack.Slack"]
"obs" = ["flatpak", "run", "com.obsproject.Studio"]

# Simple command remapping
"com.mitchellh.ghostty" = ["ghostty"]
"org.wezfurlong.wezterm" = ["wezterm"]

# Commands with arguments
"firefox-custom" = ["firefox", "--profile", "default-release"]

# Terminal state recovery — restore running commands inside terminals
[terminal_state]
enabled = true
terminal_app_ids = ["kitty", "foot", "org.wezfurlong.wezterm", "com.mitchellh.ghostty", "alacritty"]
shell_names = ["fish", "bash", "zsh", "sh", "dash", "-fish", "-bash", "-zsh", "-sh", "sudo", "doas"]
helper_names = ["kitten"]
max_walk_depth = 20
```

If no configuration file exists, one will be created with example mappings.

### Terminal State Recovery

When enabled, the session manager walks the process tree of terminal windows via `/proc` to find foreground child processes (skipping shells like `fish`, `bash`, `zsh`). On restore, it re-launches the terminal with the original command and working directory.

For example, if `kitty` was running `btop` in `/home/user/projects`, the restored command becomes:

```bash
kitty --directory /home/user/projects sh -c "'btop'; exec $SHELL"
```

Terminal-specific flags are handled automatically:
- **kitty**: `--directory`, positional command
- **foot**: `--working-directory`, positional command
- **wezterm**: `start --cwd ... -- sh -c ...`
- **ghostty**: `--working-directory=...`, `-e sh -c ...`
- **alacritty**: `--working-directory`, `-e sh -c ...`

This feature is Linux-only. On other platforms, `terminal_state` is always `None`.

## Installation

### Using Nix Flakes

```nix
{
  description = "Your NixOS configuration";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    niri-session-manager.url = "github:MTeaHead/niri-session-manager";
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

## Storage

Session data and backups are stored in:
- Session file: `$XDG_DATA_HOME/niri-session-manager/session.json`
- Backups: `$XDG_DATA_HOME/niri-session-manager/session-{timestamp}.bak`
- Configuration: `$XDG_CONFIG_HOME/niri-session-manager/config.toml`

## Future (when IPC supports it)
- Grab window size and further details for better placement when restoring windows
- Configurable per-terminal restore command templates
- `--dry-run` flag to preview restore without spawning
