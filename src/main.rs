use anyhow::{Context, Result};
use niri_ipc::{Action, Reply, Request, Response, Window, WorkspaceReferenceArg, socket::Socket};
use std::{fs, path::PathBuf, sync::Arc};
use chrono::{Local, SecondsFormat};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
    spawn,
    sync::Notify,
    time::Duration,
    time::sleep,
};
use serde::{Serialize, Deserialize};
use std::time::{ UNIX_EPOCH};
use clap::Parser;
use std::io::Write;
use std::collections::HashMap;
use toml;

/// Fetch the windows list
async fn get_niri_windows() -> Result<Vec<Window>> {
    let socket = Socket::connect().context("Failed to connect to Niri IPC socket")?;
    let (reply, _) = socket
        .send(Request::Windows)
        .context("Failed to retrieve windows from Niri IPC")?;

    match reply {
        Reply::Ok(Response::Windows(windows)) => Ok(windows),
        Reply::Err(error_msg) => anyhow::bail!("Niri IPC returned an error: {}", error_msg),
        _ => anyhow::bail!("Unexpected reply type from Niri"),
    }
}

/// fetch the session file path
fn get_session_file_path() -> Result<PathBuf> {
    let mut session_dir =
        dirs::data_dir().context("Failed to locate data directory (XDG_DATA_HOME)")?;
    session_dir.push("niri-session-manager");
    fs::create_dir_all(&session_dir).context("Failed to create session directory")?;
    Ok(session_dir.join("session.json"))
}

// Define a struct that doesn't include the `title` field
#[derive(Serialize, Deserialize)]
struct WindowWithoutTitle {
    id: u64,
    app_id: String,
    workspace_id: Option<u64>,
    is_focused: bool,
}

/// Save the session to a file
async fn save_session(file_path: &PathBuf) -> Result<()> {
    let windows = get_niri_windows().await?;

    // Create a new list of windows without the `title` field
    let windows_without_title: Vec<WindowWithoutTitle> = windows.into_iter().map(|window| {
        WindowWithoutTitle {
            id: window.id,
            app_id: window.app_id.unwrap_or_default(),
            workspace_id: window.workspace_id,
            is_focused: window.is_focused,
        }
    }).collect();

//    let json_data = serde_json::to_string_pretty(&windows).context("Failed to serialize window data")?;
    // Serialize the modified windows list to JSON
    let json_data = serde_json::to_string_pretty(&windows_without_title)
        .context("Failed to serialize window data")?;

    fs::write(file_path, json_data).context("Failed to write session file")?;
    log(&format!("Session saved to {}", file_path.display()));
    Ok(())
}

/// Restore saved session with retry logic
async fn restore_session(file_path: &PathBuf, config: &Config) -> Result<()> {
    for attempt in 1..=config.retry_attempts {
        match restore_session_internal(file_path, config).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt < config.retry_attempts => {
                eprintln!(
                    "Attempt {} failed: {}. Retrying in {} seconds...", 
                    attempt, e, config.retry_delay
                );
                sleep(Duration::from_secs(config.retry_delay)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// App launch configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    #[serde(default)]
    app_mappings: HashMap<String, Vec<String>>,
    #[serde(default, rename = "single_instance_apps")]
    single_instance: SingleInstanceAppsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SingleInstanceAppsConfig {
    #[serde(default)]
    apps: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app_mappings: HashMap::new(),
            single_instance: SingleInstanceAppsConfig::default(),
        }
    }
}

/// Load app configuration from TOML file
fn load_app_config() -> Result<AppConfig> {
    let mut config_path = dirs::config_dir()
        .context("Failed to locate config directory")?;
    config_path.push("niri-session-manager");
    config_path.push("config.toml");

    if !config_path.exists() {
        // Create default config if it doesn't exist
        fs::create_dir_all(config_path.parent().unwrap())?;
        fs::write(&config_path, r#"# Niri Session Manager Configuration

# Apps that should only have one instance
[single_instance_apps] 
apps = [
    "firefox",
    "zen"
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
"#)?;
        return Ok(AppConfig::default());
    }

    let config_str = fs::read_to_string(&config_path)
        .context("Failed to read config file")?;
    
    let config: AppConfig = toml::from_str(&config_str)
        .context("Failed to parse config file")?;
    //log(&format!("Single-instance apps: {:?}", config.single_instance.apps));
    //log(&format!("Loaded configuration with {} app mappings", config.app_mappings.len()));
    //log(&format!("{:?} app mappings", config.app_mappings));
    Ok(config)
}

/// Update restore_session_internal to use app mappings
async fn restore_session_internal(file_path: &PathBuf, config: &Config) -> Result<()> {
    if !file_path.exists() {
        log(&format!("No previous session found at {}", file_path.display()));
        log("Building new session file");
        save_session(&file_path).await?;
        return Ok(());
    }

    let session_data = fs::read_to_string(file_path).context("Failed to read session file")?;
    if session_data.trim().is_empty() {
        log(&format!("Session file at {} is empty", file_path.display()));
        return Ok(());
    }
    let windows: Vec<Window> =
        serde_json::from_str(&session_data).context("Failed to parse session JSON")?;

    let current_windows = get_niri_windows().await?;
    let mut handles = Vec::new();

    // Load app configuration
    let app_config = load_app_config()?;

    let mut spawned_apps = std::collections::HashSet::new();
    
    for window in windows {

        let app_id = window.app_id.clone().unwrap_or_default();

        let should_skip = current_windows.iter().any(|w| w.app_id == Some(app_id.clone()))
            || spawned_apps.contains(&app_id);
        
        let workspace_id = window.workspace_id;

        // Check if app is single-instance and already running
        if app_config.single_instance.apps.contains(&app_id) && should_skip {
            log(&format!("Skipping single-instance app: {}", app_id));
            continue;
        }

        if app_config.single_instance.apps.contains(&app_id) {
            spawned_apps.insert(app_id.clone());
        }

        // Get command from app mappings or use app_id as fallback
        let command = app_config.app_mappings
            .get(&app_id)
            .cloned()
            .unwrap_or_else(|| vec![app_id.clone()]);

        let spawn_timeout = config.spawn_timeout;
        let handle = spawn(async move {
            let spawn_socket = Socket::connect().context("Failed to connect to Niri IPC socket")?;
            let (reply, _) = spawn_socket
                .send(Request::Action(Action::Spawn {
                    command: command.clone(),
                }))
                .context("Failed to send spawn request")?;

            if let Reply::Ok(Response::Handled) = reply {
                // Use configured spawn timeout
                for _ in 0..spawn_timeout * 2 {
                    sleep(Duration::from_millis(500)).await;
                    let new_windows = get_niri_windows().await?;
                    if let Some(new_window) = new_windows
                        .iter()
                        .find(|w| w.app_id == Some(app_id.clone()))
                    {
                        let move_socket =
                            Socket::connect().context("Failed to connect to Niri IPC socket")?;
                        let _ = move_socket
                            .send(Request::Action(Action::MoveWindowToWorkspace {
                                window_id: Some(new_window.id),
                                reference: WorkspaceReferenceArg::Id(
                                    workspace_id.unwrap_or_default(),
                                ),
                            }))
                            .context("Failed to move window to the workspace")?;
                        break;
                    }
                }
            } else {
                log(&format!("Failed to spawn app: {} using command: {:?}", 
                    app_id, command));
            }

            Result::<()>::Ok(())
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete.
    for handle in handles {
        handle.await.context("Task execution failed")??;
    }

    log("Session restored.");
    // Clean up the session file after restoring.
    //fs::remove_file(file_path).context("Failed to delete session file")?;
    //println!("Session file cleaned up.");
    Ok(())
}

/// Handle shutdown signals and notify the main function.
async fn handle_shutdown_signals(shutdown_signal: Arc<Notify>) {
    let mut term_signal = signal(SignalKind::terminate()).expect("Failed to listen for SIGTERM");
    let mut int_signal = signal(SignalKind::interrupt()).expect("Failed to listen for SIGINT");
    let mut quit_signal = signal(SignalKind::quit()).expect("Failed to listen for SIGQUIT");

    select! {
        _ = term_signal.recv() => {
            log("Received SIGTERM signal");
            shutdown_signal.notify_waiters();
        },
        _ = int_signal.recv() => {
            log("Received SIGINT signal");
            shutdown_signal.notify_waiters();
        },
        _ = quit_signal.recv() => {
            log("Received SIGQUIT signal");
            shutdown_signal.notify_waiters();
        },
    }
}

/// Periodically save the session based on configured interval
async fn periodic_save_session(
    file_path: PathBuf,
    shutdown_signal: Arc<Notify>,
    config: Config
) {
    let interval = Duration::from_secs(config.save_interval * 60); // Convert minutes to seconds
    let session_dir = file_path.parent().unwrap_or(&file_path).to_path_buf();

    log(&format!("Starting periodic save task (interval: {} minutes)", config.save_interval));

    loop {
        select! {
            _ = sleep(interval) => {
                if let Err(e) = save_session_with_backup(&file_path, &config).await {
                    log_error(&format!("Error saving session: {}", e));
                }
                // Cleanup old backups after each save
                if let Err(e) = cleanup_old_backups(&session_dir, config.max_backup_count) {
                    log_error(&format!("Error cleaning up old backups: {}", e));
                }
            },
            _ = shutdown_signal.notified() => {
                log("Shutting down, stopping periodic session saves");
                if let Err(e) = save_session_with_backup(&file_path, &config).await {
                    log_error(&format!("Error saving session: {}", e));
                } else {
                    log("Final session saved");
                }
                break;
            }
        }
    }
}

async fn save_session_with_backup(file_path: &PathBuf, config: &Config) -> Result<()> {
    create_backup(file_path)?;
    
    // Cleanup old backups after creating a new one
    if let Some(session_dir) = file_path.parent() {
        cleanup_old_backups(&session_dir.to_path_buf(), config.max_backup_count)?;
    }
    
    save_session(file_path).await
}

/// Create a timestamped backup of the file
fn create_backup(file_path: &PathBuf) -> Result<()> {
    if file_path.exists() {
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let backup_file_name = format!(
            "{}-{}.bak",
            file_path.file_stem().unwrap_or_default().to_string_lossy(),
            timestamp
        );
        let mut backup_path = file_path.clone();
        backup_path.set_file_name(backup_file_name);
        fs::copy(file_path, &backup_path).context("Failed to create backup file")?;
        log(&format!("Backup created at {}", backup_path.display()));
    }
    Ok(())
}

/// Clean up old backup files, keeping only the most recent N backups
fn cleanup_old_backups(session_dir: &PathBuf, keep_count: usize) -> Result<()> {
    // Get all backup files
    let mut backups: Vec<_> = fs::read_dir(session_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();
    
    if backups.len() <= keep_count {
        return Ok(());
    }

    // Sort by modification time, newest first
    backups.sort_by(|a, b| {
        b.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH)
            .cmp(
                &a.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH)
            )
    });

    // Remove older backups
    for backup in backups.iter().skip(keep_count) {
        if let Err(e) = fs::remove_file(backup.path()) {
            log_error(&format!("Failed to remove old backup {}: {}", 
                backup.path().display(), e));
        } else {
            log(&format!("Removed old backup: {}", backup.path().display()));
        }
    }

    Ok(())
}

/// CLI Arguments for niri-session-manager
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Config {
    /// Save interval in minutes
    #[arg(long, default_value = "15")]
    save_interval: u64,

    /// Maximum number of backup files to keep
    #[arg(long, default_value = "5")]
    max_backup_count: usize,

    /// Timeout in seconds when spawning windows
    #[arg(long, default_value = "5")]
    spawn_timeout: u64,

    /// Number of restore attempts
    #[arg(long, default_value = "3")]
    retry_attempts: u32,

    /// Delay between retry attempts in seconds
    #[arg(long, default_value = "2")]
    retry_delay: u64,
}

// Update log function to handle format strings
fn log(message: &str) {
    println!("{message}");
    std::io::stdout().flush().unwrap_or_default();
}

// Update error logging
fn log_error(message: &str) {
    eprintln!("{}", message);
    std::io::stderr().flush().unwrap_or_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command line arguments
    let config = Config::parse();
    
    log("Starting niri-session-manager");
    let session_file_path = get_session_file_path()?;
    let shutdown_signal = Arc::new(Notify::new());

    // Start the periodic save task with config
    let shutdown_signal_clone = Arc::clone(&shutdown_signal);
    let save_task = spawn(periodic_save_session(
        session_file_path.clone(),
        shutdown_signal_clone,
        config.clone()
    ));

    // Restore session with config
    log("Restoring previous session");
    restore_session(&session_file_path, &config).await?;
    
    let shutdown_signal_clone = Arc::clone(&shutdown_signal);
    let signal_task = spawn(handle_shutdown_signals(shutdown_signal_clone));

    // Wait for shutdown signal and tasks to complete
    shutdown_signal.notified().await;
    
    // Wait for tasks to finish with timeout
    let timeout = Duration::from_secs(5);
    select! {
        _ = save_task => log("Save task completed"),
        _ = signal_task => log("Signal handler completed"),
        _ = sleep(timeout) => log("Shutdown timed out"),
    }

    log("Shutdown complete");
    Ok(())
}
