mod proc;

use anyhow::{Context, Result};
use chrono::{Local, SecondsFormat};
use clap::Parser;
use niri_ipc::{
    Action, Reply, Request, Response, Window, Workspace, WorkspaceReferenceArg, socket::Socket,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::UNIX_EPOCH;
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
    spawn,
    sync::Notify,
    task::spawn_blocking,
    time::Duration,
    time::sleep,
};

async fn niri_send(request: Request) -> Result<Response> {
    let mut socket = Socket::connect().context("Failed to connect to Niri IPC socket")?;
    let reply = socket
        .send(request)
        .context("Failed to communicate with Niri IPC")?;
    match reply {
        Reply::Ok(response) => Ok(response),
        Reply::Err(error_msg) => anyhow::bail!("Niri IPC returned an error: {}", error_msg),
    }
}

async fn get_niri_windows() -> Result<Vec<Window>> {
    match niri_send(Request::Windows).await? {
        Response::Windows(windows) => Ok(windows),
        _ => anyhow::bail!("Expected Windows response from Niri"),
    }
}

async fn get_niri_workspaces() -> Result<Vec<Workspace>> {
    match niri_send(Request::Workspaces).await? {
        Response::Workspaces(workspaces) => Ok(workspaces),
        _ => anyhow::bail!("Expected Workspaces response from Niri"),
    }
}

fn get_session_file_path() -> Result<std::path::PathBuf> {
    let mut session_dir =
        dirs::data_dir().context("Failed to locate data directory (XDG_DATA_HOME)")?;
    session_dir.push("niri-session-manager");
    fs::create_dir_all(&session_dir).context("Failed to create session directory")?;
    Ok(session_dir.join("session.json"))
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedWindow {
    id: u64,
    app_id: String,
    #[serde(default)]
    workspace_idx: Option<u8>,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    workspace_output: Option<String>,
    is_focused: bool,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    terminal_state: Option<TerminalState>,
    #[serde(default, skip_serializing)]
    workspace_id: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
enum ChildCommand {
    Args(Vec<String>),
    Legacy(String),
}

impl ChildCommand {
    fn to_args(&self) -> Vec<String> {
        match self {
            ChildCommand::Args(args) => args.clone(),
            ChildCommand::Legacy(s) => s.split_whitespace().map(String::from).collect(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TerminalState {
    child_command: Option<ChildCommand>,
    child_cwd: Option<String>,
}

const SESSION_FORMAT_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
struct VersionedSession {
    version: u32,
    windows: Vec<SavedWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum SessionData {
    Versioned(VersionedSession),
    Legacy(Vec<SavedWindow>),
}

impl SessionData {
    fn into_windows(self) -> Vec<SavedWindow> {
        match self {
            SessionData::Versioned(v) => v.windows,
            SessionData::Legacy(windows) => windows,
        }
    }

    fn is_legacy(&self) -> bool {
        matches!(self, SessionData::Legacy(_))
    }
}

async fn restore_session(file_path: &Path, config: &Config) -> Result<()> {
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

fn default_enabled() -> bool {
    true
}
fn default_terminal_app_ids() -> Vec<String> {
    vec![
        "kitty".into(),
        "foot".into(),
        "org.wezfurlong.wezterm".into(),
        "com.mitchellh.ghostty".into(),
        "alacritty".into(),
    ]
}
fn default_shell_names() -> Vec<String> {
    vec![
        "fish".into(),
        "bash".into(),
        "zsh".into(),
        "sh".into(),
        "dash".into(),
        "-fish".into(),
        "-bash".into(),
        "-zsh".into(),
        "-sh".into(),
        "sudo".into(),
        "doas".into(),
    ]
}
fn default_helper_names() -> Vec<String> {
    vec!["kitten".into()]
}
fn default_max_walk_depth() -> u32 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TerminalStateConfig {
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_terminal_app_ids")]
    terminal_app_ids: Vec<String>,
    #[serde(default = "default_shell_names")]
    shell_names: Vec<String>,
    #[serde(default = "default_helper_names")]
    helper_names: Vec<String>,
    #[serde(default = "default_max_walk_depth")]
    max_walk_depth: u32,
}

impl Default for TerminalStateConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            terminal_app_ids: default_terminal_app_ids(),
            shell_names: default_shell_names(),
            helper_names: default_helper_names(),
            max_walk_depth: default_max_walk_depth(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SingleInstanceAppsConfig {
    #[serde(default)]
    apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SkipAppsConfig {
    #[serde(default)]
    apps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    app_mappings: HashMap<String, Vec<String>>,
    #[serde(default, rename = "single_instance_apps")]
    single_instance: SingleInstanceAppsConfig,
    #[serde(default, rename = "skip_apps")]
    skip_apps: SkipAppsConfig,
    #[serde(default)]
    terminal_state: TerminalStateConfig,
}

fn load_app_config() -> Result<AppConfig> {
    let mut config_path = dirs::config_dir().context("Failed to locate config directory")?;
    config_path.push("niri-session-manager");
    config_path.push("config.toml");

    if !config_path.exists() {
        fs::create_dir_all(config_path.parent().unwrap())?;
        fs::write(
            &config_path,
            r#"# Niri Session Manager Configuration

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

# Terminal state recovery — restore running commands inside terminals
[terminal_state]
enabled = true
terminal_app_ids = ["kitty", "foot", "org.wezfurlong.wezterm", "com.mitchellh.ghostty", "alacritty"]
shell_names = ["fish", "bash", "zsh", "sh", "dash", "-fish", "-bash", "-zsh", "-sh", "sudo", "doas"]
helper_names = ["kitten"]
max_walk_depth = 20
"#,
        )?;
        return Ok(AppConfig::default());
    }

    let config_str = fs::read_to_string(&config_path).context("Failed to read config file")?;

    let config: AppConfig = toml::from_str(&config_str).context("Failed to parse config file")?;
    Ok(config)
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn get_shell_from_passwd() -> Option<String> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let uid_line = status.lines().find(|l| l.starts_with("Uid:"))?;
    let uid = uid_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u32>().ok())?;

    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid) {
            let shell = fields[6].trim();
            if !shell.is_empty() {
                return Some(shell.to_string());
            }
        }
    }
    None
}

fn get_restore_shell() -> String {
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return shell;
    }
    #[cfg(target_os = "linux")]
    if let Some(shell) = get_shell_from_passwd() {
        return shell;
    }
    "/bin/sh".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CwdFlag {
    Separated(&'static str),
    Joined(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Kitty,
    Foot,
    Wezterm,
    Ghostty,
    Alacritty,
    Generic,
}

impl TerminalProfile {
    fn from_executable(name: &str) -> Self {
        match name {
            "kitty" => Self::Kitty,
            "foot" => Self::Foot,
            "wezterm" => Self::Wezterm,
            "ghostty" => Self::Ghostty,
            "alacritty" => Self::Alacritty,
            _ => Self::Generic,
        }
    }

    fn needs_start_subcommand(&self) -> bool {
        matches!(self, Self::Wezterm)
    }

    fn cwd_flag(&self) -> CwdFlag {
        match self {
            Self::Kitty => CwdFlag::Separated("--directory"),
            Self::Foot => CwdFlag::Separated("--working-directory"),
            Self::Wezterm => CwdFlag::Separated("--cwd"),
            Self::Ghostty => CwdFlag::Joined("--working-directory="),
            Self::Alacritty | Self::Generic => CwdFlag::Separated("--working-directory"),
        }
    }

    fn cmd_flag(&self) -> Option<&'static str> {
        match self {
            Self::Kitty | Self::Foot => None,
            Self::Wezterm => Some("--"),
            Self::Ghostty | Self::Alacritty | Self::Generic => Some("-e"),
        }
    }
}

fn resolve_executable_name(app_id: &str, app_mappings: &HashMap<String, Vec<String>>) -> String {
    app_mappings
        .get(app_id)
        .and_then(|args| args.first())
        .cloned()
        .unwrap_or_else(|| app_id.to_string())
}

fn build_terminal_restore_command(
    launch_prefix: &[String],
    profile: &TerminalProfile,
    child_cmd: &[String],
    child_cwd: &Option<String>,
) -> Vec<String> {
    let mut cmd: Vec<String> = launch_prefix.to_vec();

    if profile.needs_start_subcommand() {
        cmd.push("start".to_string());
    }

    let cwd_flag = child_cwd
        .as_ref()
        .filter(|cwd| !cwd.is_empty() && **cwd != std::env::var("HOME").unwrap_or_default());

    if let Some(cwd) = cwd_flag {
        match profile.cwd_flag() {
            CwdFlag::Separated(flag) => {
                cmd.push(flag.to_string());
                cmd.push(cwd.clone());
            }
            CwdFlag::Joined(flag) => {
                cmd.push(format!("{}{}", flag, cwd));
            }
        }
    }

    if let Some(flag) = profile.cmd_flag() {
        cmd.push(flag.to_string());
    }

    let escaped_cmd: String = child_cmd
        .iter()
        .map(|arg| shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let restore_shell = get_restore_shell();
    cmd.push("sh".to_string());
    cmd.push("-c".to_string());
    cmd.push(format!(
        "{}; exec {}",
        escaped_cmd,
        shell_escape(&restore_shell)
    ));

    cmd
}

fn build_spawn_command(
    app_id: &str,
    saved_window: &SavedWindow,
    app_mappings: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mapped = app_mappings
        .get(app_id)
        .cloned()
        .unwrap_or_else(|| vec![app_id.to_string()]);

    if let Some(ts) = &saved_window.terminal_state
        && let Some(child_cmd) = &ts.child_command
    {
        let args = child_cmd.to_args();
        if !args.is_empty() {
            let exec_name = resolve_executable_name(app_id, app_mappings);
            let profile = TerminalProfile::from_executable(&exec_name);
            return build_terminal_restore_command(&mapped, &profile, &args, &ts.child_cwd);
        }
    }

    mapped
}

async fn resolve_terminal_state(
    pid: u32,
    config: &TerminalStateConfig,
) -> Option<(Vec<String>, String)> {
    let shell_names = config.shell_names.clone();
    let helper_names = config.helper_names.clone();
    let max_depth = config.max_walk_depth;
    spawn_blocking(move || proc::resolve_child_process(pid, &shell_names, &helper_names, max_depth))
        .await
        .ok()
        .flatten()
}

async fn save_session_with_terminal_state(file_path: &Path, app_config: &AppConfig) -> Result<()> {
    let windows = get_niri_windows().await?;
    let workspaces = get_niri_workspaces().await?;
    let terminal_config = &app_config.terminal_state;

    let mut saved_windows = Vec::with_capacity(windows.len());

    for window in &windows {
        let ws = workspaces
            .iter()
            .find(|w| window.workspace_id == Some(w.id));
        let app_id = window.app_id.clone().unwrap_or_default();
        let pid = window
            .pid
            .and_then(|p| if p > 0 { Some(p as u32) } else { None });

        let terminal_state = if terminal_config.enabled {
            match pid {
                Some(pid) if terminal_config.terminal_app_ids.contains(&app_id) => {
                    resolve_terminal_state(pid, terminal_config).await.map(
                        |(child_command, child_cwd)| TerminalState {
                            child_command: Some(ChildCommand::Args(child_command)),
                            child_cwd: Some(child_cwd),
                        },
                    )
                }
                _ => None,
            }
        } else {
            None
        };

        saved_windows.push(SavedWindow {
            id: window.id,
            app_id: app_id.clone(),
            workspace_idx: ws.map(|w| w.idx),
            workspace_name: ws.and_then(|w| w.name.clone()),
            workspace_output: ws.and_then(|w| w.output.clone()),
            is_focused: window.is_focused,
            pid,
            terminal_state,
            workspace_id: None,
        });
    }

    let session = VersionedSession {
        version: SESSION_FORMAT_VERSION,
        windows: saved_windows,
    };
    let json_data =
        serde_json::to_string_pretty(&session).context("Failed to serialize window data")?;

    fs::write(file_path, json_data).context("Failed to write session file")?;
    log(&format!("Session saved to {}", file_path.display()));
    Ok(())
}

async fn restore_session_internal(file_path: &Path, config: &Config) -> Result<()> {
    if !file_path.exists() {
        log(&format!(
            "No previous session found at {}",
            file_path.display()
        ));
        log("Building new session file");
        let app_config = load_app_config()?;
        save_session_with_terminal_state(file_path, &app_config).await?;
        return Ok(());
    }

    let session_data = fs::read_to_string(file_path).context("Failed to read session file")?;
    if session_data.trim().is_empty() {
        log(&format!("Session file at {} is empty", file_path.display()));
        return Ok(());
    }
    let session: SessionData =
        serde_json::from_str(&session_data).context("Failed to parse session JSON")?;
    if session.is_legacy() {
        log(
            "Warning: session file uses legacy format (no version field). Consider re-saving to upgrade.",
        );
    }
    let mut saved_windows = session.into_windows();
    let has_legacy_workspace_id = saved_windows.iter().any(|w| w.workspace_id.is_some());
    if has_legacy_workspace_id {
        log(
            "Warning: session file contains deprecated 'workspace_id' field. Workspace info was lost. Re-save to upgrade format.",
        );
    }
    saved_windows.sort_by_key(|w| w.workspace_idx.unwrap_or(0));

    let current_windows = get_niri_windows().await?;
    let claimed_window_ids: Arc<Mutex<HashSet<u64>>> =
        Arc::new(Mutex::new(current_windows.iter().map(|w| w.id).collect()));
    let mut handles = Vec::new();

    let app_config = load_app_config()?;

    let mut spawned_apps = HashSet::new();

    for saved_window in saved_windows {
        let app_id = saved_window.app_id.clone();

        if app_config.skip_apps.apps.contains(&app_id) {
            log(&format!("Skipping app: {}", app_id));
            continue;
        }

        let should_skip = current_windows
            .iter()
            .any(|w| w.app_id == Some(app_id.clone()))
            || spawned_apps.contains(&app_id);

        let workspace_idx = saved_window.workspace_idx;
        let workspace_name = saved_window.workspace_name.clone();
        let workspace_output = saved_window.workspace_output.clone();

        if app_config.single_instance.apps.contains(&app_id) && should_skip {
            log(&format!("Skipping single-instance app: {}", app_id));
            continue;
        }

        if app_config.single_instance.apps.contains(&app_id) {
            spawned_apps.insert(app_id.clone());
        }

        let command = build_spawn_command(&app_id, &saved_window, &app_config.app_mappings);

        let spawn_timeout = config.spawn_timeout;
        let claimed_window_ids = Arc::clone(&claimed_window_ids);
        let handle = spawn(async move {
            let mut spawn_socket =
                Socket::connect().context("Failed to connect to Niri IPC socket")?;
            let reply = spawn_socket
                .send(Request::Action(Action::Spawn {
                    command: command.clone(),
                }))
                .context("Failed to send spawn request")?;

            if let Reply::Ok(Response::Handled) = reply {
                for _ in 0..spawn_timeout * 2 {
                    sleep(Duration::from_millis(500)).await;
                    let new_windows = get_niri_windows().await?;
                    let win_id = {
                        let mut claimed = claimed_window_ids.lock().unwrap();
                        new_windows
                            .iter()
                            .find(|w| w.app_id == Some(app_id.clone()) && !claimed.contains(&w.id))
                            .map(|w| {
                                claimed.insert(w.id);
                                w.id
                            })
                    };
                    if let Some(win_id) = win_id {
                        let mut move_socket =
                            Socket::connect().context("Failed to connect to Niri IPC socket")?;

                        if let Some(output) = &workspace_output {
                            let result =
                                move_socket.send(Request::Action(Action::MoveWindowToMonitor {
                                    id: Some(win_id),
                                    output: output.clone(),
                                }));
                            if let Err(e) = &result {
                                log_error(&format!(
                                    "Warning: failed to move window {} to monitor {}: {:?}",
                                    win_id, output, e
                                ));
                            }
                        }

                        let workspace_reference =
                            if let Some(name) = workspace_name.as_ref().filter(|n| !n.is_empty()) {
                                WorkspaceReferenceArg::Name(name.clone())
                            } else {
                                WorkspaceReferenceArg::Index(workspace_idx.unwrap_or(0))
                            };

                        if let Err(e) =
                            move_socket.send(Request::Action(Action::MoveWindowToWorkspace {
                                window_id: Some(win_id),
                                reference: workspace_reference,
                                focus: false,
                            }))
                        {
                            log_error(&format!(
                                "Warning: failed to move window {} to workspace: {:?}",
                                win_id, e
                            ));
                        }
                        break;
                    }
                }
            } else {
                log(&format!(
                    "Failed to spawn app: {} using command: {:?}",
                    app_id, command
                ));
            }

            Result::<()>::Ok(())
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.context("Task execution failed")??;
    }

    log("Session restored.");
    Ok(())
}

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

async fn periodic_save_session(
    file_path: std::path::PathBuf,
    shutdown_signal: Arc<Notify>,
    config: Config,
) {
    let interval = Duration::from_secs(config.save_interval * 60);
    let session_dir = file_path.parent().unwrap_or(&file_path).to_path_buf();

    log(&format!(
        "Starting periodic save task (interval: {} minutes)",
        config.save_interval
    ));

    loop {
        select! {
            _ = sleep(interval) => {
                if let Err(e) = save_session_with_backup(&file_path, &config).await {
                    log_error(&format!("Error saving session: {}", e));
                }
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

async fn save_session_with_backup(file_path: &Path, config: &Config) -> Result<()> {
    create_backup(file_path)?;

    if let Some(session_dir) = file_path.parent() {
        cleanup_old_backups(session_dir, config.max_backup_count)?;
    }

    let app_config = load_app_config()?;
    save_session_with_terminal_state(file_path, &app_config).await
}

fn create_backup(file_path: &Path) -> Result<()> {
    if file_path.exists() {
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let backup_file_name = format!(
            "{}-{}.bak",
            file_path.file_stem().unwrap_or_default().to_string_lossy(),
            timestamp
        );
        let mut backup_path = file_path.to_path_buf();
        backup_path.set_file_name(backup_file_name);
        fs::copy(file_path, &backup_path).context("Failed to create backup file")?;
        log(&format!("Backup created at {}", backup_path.display()));
    }
    Ok(())
}

fn cleanup_old_backups(session_dir: &Path, keep_count: usize) -> Result<()> {
    let mut backups: Vec<_> = fs::read_dir(session_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".bak"))
                .unwrap_or(false)
        })
        .collect();

    if backups.len() <= keep_count {
        return Ok(());
    }

    backups.sort_by(|a, b| {
        b.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH)
            .cmp(
                &a.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH),
            )
    });

    for backup in backups.iter().skip(keep_count) {
        if let Err(e) = fs::remove_file(backup.path()) {
            log_error(&format!(
                "Failed to remove old backup {}: {}",
                backup.path().display(),
                e
            ));
        } else {
            log(&format!("Removed old backup: {}", backup.path().display()));
        }
    }

    Ok(())
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Config {
    #[arg(long, default_value = "15")]
    save_interval: u64,

    #[arg(long, default_value = "5")]
    max_backup_count: usize,

    #[arg(long, default_value = "5")]
    spawn_timeout: u64,

    #[arg(long, default_value = "3")]
    retry_attempts: u32,

    #[arg(long, default_value = "2")]
    retry_delay: u64,
}

fn log(message: &str) {
    println!("{message}");
    std::io::stdout().flush().unwrap_or_default();
}

fn log_error(message: &str) {
    eprintln!("{}", message);
    std::io::stderr().flush().unwrap_or_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();

    log("Starting niri-session-manager");
    let session_file_path = get_session_file_path()?;
    let shutdown_signal = Arc::new(Notify::new());

    let shutdown_signal_clone = Arc::clone(&shutdown_signal);
    let save_task = spawn(periodic_save_session(
        session_file_path.clone(),
        shutdown_signal_clone,
        config.clone(),
    ));

    log("Restoring previous session");
    restore_session(&session_file_path, &config).await?;

    let shutdown_signal_clone = Arc::clone(&shutdown_signal);
    let signal_task = spawn(handle_shutdown_signals(shutdown_signal_clone));

    shutdown_signal.notified().await;

    let timeout = Duration::from_secs(5);
    select! {
        _ = save_task => log("Save task completed"),
        _ = signal_task => log("Signal handler completed"),
        _ = sleep(timeout) => log("Shutdown timed out"),
    }

    log("Shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_exec_suffix(child_cmd: &str) -> String {
        let shell = get_restore_shell();
        format!("{}; exec {}", shell_escape(child_cmd), shell_escape(&shell))
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("btop"), "'btop'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("nvim /path/to file"), "'nvim /path/to file'");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape("echo 'hello'"), "'echo '\\''hello'\\'''");
    }

    #[test]
    fn shell_escape_with_semicolons() {
        assert_eq!(shell_escape("cmd; rm -rf /"), "'cmd; rm -rf /'");
    }

    #[test]
    fn shell_escape_with_dollar() {
        assert_eq!(shell_escape("echo $HOME"), "'echo $HOME'");
    }

    #[test]
    fn shell_escape_with_backticks() {
        assert_eq!(shell_escape("echo `whoami`"), "'echo `whoami`'");
    }

    #[test]
    fn resolve_executable_from_mappings() {
        let mut mappings = HashMap::new();
        mappings.insert(
            "com.mitchellh.ghostty".to_string(),
            vec!["ghostty".to_string()],
        );
        mappings.insert(
            "org.wezfurlong.wezterm".to_string(),
            vec!["wezterm".to_string()],
        );

        assert_eq!(
            resolve_executable_name("com.mitchellh.ghostty", &mappings),
            "ghostty"
        );
        assert_eq!(
            resolve_executable_name("org.wezfurlong.wezterm", &mappings),
            "wezterm"
        );
        assert_eq!(resolve_executable_name("kitty", &mappings), "kitty");
        assert_eq!(resolve_executable_name("unknown", &mappings), "unknown");
    }

    #[test]
    fn terminal_profile_kitty() {
        let p = TerminalProfile::from_executable("kitty");
        assert!(!p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Separated("--directory"));
        assert_eq!(p.cmd_flag(), None);
    }

    #[test]
    fn terminal_profile_foot() {
        let p = TerminalProfile::from_executable("foot");
        assert!(!p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Separated("--working-directory"));
        assert_eq!(p.cmd_flag(), None);
    }

    #[test]
    fn terminal_profile_wezterm() {
        let p = TerminalProfile::from_executable("wezterm");
        assert!(p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Separated("--cwd"));
        assert_eq!(p.cmd_flag(), Some("--"));
    }

    #[test]
    fn terminal_profile_ghostty() {
        let p = TerminalProfile::from_executable("ghostty");
        assert!(!p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Joined("--working-directory="));
        assert_eq!(p.cmd_flag(), Some("-e"));
    }

    #[test]
    fn terminal_profile_alacritty() {
        let p = TerminalProfile::from_executable("alacritty");
        assert!(!p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Separated("--working-directory"));
        assert_eq!(p.cmd_flag(), Some("-e"));
    }

    #[test]
    fn terminal_profile_generic() {
        let p = TerminalProfile::from_executable("unknown-terminal");
        assert!(!p.needs_start_subcommand());
        assert_eq!(p.cwd_flag(), CwdFlag::Separated("--working-directory"));
        assert_eq!(p.cmd_flag(), Some("-e"));
    }

    fn assert_restore_command(cmd: &[String], expected_prefix: &[&str], child_cmd: &str) {
        for (i, expected) in expected_prefix.iter().enumerate() {
            assert_eq!(cmd[i], *expected);
        }
        assert_eq!(cmd[expected_prefix.len()], expected_exec_suffix(child_cmd));
    }

    #[test]
    fn build_restore_kitty_with_cwd() {
        let profile = TerminalProfile::Kitty;
        let cmd = build_terminal_restore_command(
            &["kitty".to_string()],
            &profile,
            &["btop".to_string()],
            &Some("/home/user/projects".to_string()),
        );
        assert_restore_command(
            &cmd,
            &["kitty", "--directory", "/home/user/projects", "sh", "-c"],
            "btop",
        );
    }

    #[test]
    fn build_restore_kitty_without_cwd() {
        let profile = TerminalProfile::Kitty;
        let home = std::env::var("HOME").unwrap_or_default();
        let cmd = build_terminal_restore_command(
            &["kitty".to_string()],
            &profile,
            &["btop".to_string()],
            &Some(home.clone()),
        );
        assert_restore_command(&cmd, &["kitty", "sh", "-c"], "btop");
    }

    #[test]
    fn build_restore_wezterm_with_cwd() {
        let profile = TerminalProfile::Wezterm;
        let cmd = build_terminal_restore_command(
            &["wezterm".to_string()],
            &profile,
            &["btop".to_string()],
            &Some("/home/user/projects".to_string()),
        );
        assert_restore_command(
            &cmd,
            &[
                "wezterm",
                "start",
                "--cwd",
                "/home/user/projects",
                "--",
                "sh",
                "-c",
            ],
            "btop",
        );
    }

    #[test]
    fn build_restore_ghostty_with_cwd() {
        let profile = TerminalProfile::Ghostty;
        let cmd = build_terminal_restore_command(
            &["ghostty".to_string()],
            &profile,
            &["btop".to_string()],
            &Some("/home/user/projects".to_string()),
        );
        assert_restore_command(
            &cmd,
            &[
                "ghostty",
                "--working-directory=/home/user/projects",
                "-e",
                "sh",
                "-c",
            ],
            "btop",
        );
    }

    #[test]
    fn build_restore_foot_with_cwd() {
        let profile = TerminalProfile::Foot;
        let cmd = build_terminal_restore_command(
            &["foot".to_string()],
            &profile,
            &["btop".to_string()],
            &Some("/home/user/projects".to_string()),
        );
        assert_restore_command(
            &cmd,
            &[
                "foot",
                "--working-directory",
                "/home/user/projects",
                "sh",
                "-c",
            ],
            "btop",
        );
    }

    #[test]
    fn build_restore_alacritty_with_cwd() {
        let profile = TerminalProfile::Alacritty;
        let cmd = build_terminal_restore_command(
            &["alacritty".to_string()],
            &profile,
            &["btop".to_string()],
            &Some("/home/user/projects".to_string()),
        );
        assert_restore_command(
            &cmd,
            &[
                "alacritty",
                "--working-directory",
                "/home/user/projects",
                "-e",
                "sh",
                "-c",
            ],
            "btop",
        );
    }

    #[test]
    fn build_restore_with_shell_metacharacters() {
        let profile = TerminalProfile::Kitty;
        let cmd = build_terminal_restore_command(
            &["kitty".to_string()],
            &profile,
            &["echo 'hello'; rm -rf /".to_string()],
            &None,
        );
        assert_eq!(cmd[3], expected_exec_suffix("echo 'hello'; rm -rf /"));
    }

    #[test]
    fn build_restore_preserves_multi_arg_command() {
        let profile = TerminalProfile::Kitty;
        let cmd = build_terminal_restore_command(
            &["kitty".to_string()],
            &profile,
            &["nvim".to_string(), "/path/to file".to_string()],
            &None,
        );
        let expected_suffix = {
            let shell = get_restore_shell();
            format!(
                "{} {}; exec {}",
                shell_escape("nvim"),
                shell_escape("/path/to file"),
                shell_escape(&shell)
            )
        };
        assert_eq!(cmd[3], expected_suffix);
    }

    #[test]
    fn build_restore_preserves_mapped_launch_prefix() {
        let profile = TerminalProfile::Generic;
        let cmd = build_terminal_restore_command(
            &[
                "flatpak".to_string(),
                "run".to_string(),
                "org.myterm".to_string(),
            ],
            &profile,
            &["btop".to_string()],
            &None,
        );
        assert_eq!(cmd[0], "flatpak");
        assert_eq!(cmd[1], "run");
        assert_eq!(cmd[2], "org.myterm");
    }

    #[test]
    fn build_spawn_command_falls_back_to_mappings() {
        let mut mappings = HashMap::new();
        mappings.insert(
            "com.mitchellh.ghostty".to_string(),
            vec!["ghostty".to_string()],
        );

        let window = SavedWindow {
            id: 1,
            app_id: "com.mitchellh.ghostty".to_string(),
            workspace_idx: None,
            workspace_name: None,
            workspace_output: None,
            is_focused: false,
            pid: None,
            terminal_state: None,
            workspace_id: None,
        };

        let cmd = build_spawn_command("com.mitchellh.ghostty", &window, &mappings);
        assert_eq!(cmd, vec!["ghostty"]);
    }

    #[test]
    fn build_spawn_command_uses_terminal_state() {
        let mappings = HashMap::new();
        let window = SavedWindow {
            id: 1,
            app_id: "kitty".to_string(),
            workspace_idx: Some(0),
            workspace_name: None,
            workspace_output: None,
            is_focused: true,
            pid: Some(1234),
            terminal_state: Some(TerminalState {
                child_command: Some(ChildCommand::Args(vec!["btop".to_string()])),
                child_cwd: Some("/home/user".to_string()),
            }),
            workspace_id: None,
        };

        let cmd = build_spawn_command("kitty", &window, &mappings);
        assert_eq!(cmd[0], "kitty");
        assert!(cmd.contains(&expected_exec_suffix("btop")));
    }

    #[test]
    fn saved_window_deserializes_old_format_with_workspace_id() {
        let json = r#"{
            "id": 42,
            "app_id": "kitty",
            "workspace_id": 3,
            "is_focused": true
        }"#;
        let w: SavedWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.id, 42);
        assert_eq!(w.app_id, "kitty");
        assert_eq!(w.workspace_idx, None);
        assert_eq!(w.workspace_name, None);
        assert_eq!(w.workspace_output, None);
        assert!(w.terminal_state.is_none());
        assert!(w.pid.is_none());
    }

    #[test]
    fn saved_window_deserializes_new_format_with_workspace_fields() {
        let json = r#"{
            "id": 42,
            "app_id": "kitty",
            "workspace_idx": 2,
            "workspace_name": "dev",
            "workspace_output": "eDP-1",
            "is_focused": true,
            "pid": 1234,
            "terminal_state": {
                "child_command": "btop",
                "child_cwd": "/home/user"
            }
        }"#;
        let w: SavedWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.workspace_idx, Some(2));
        assert_eq!(w.workspace_name, Some("dev".to_string()));
        assert_eq!(w.workspace_output, Some("eDP-1".to_string()));
        assert_eq!(w.pid, Some(1234));
        let ts = w.terminal_state.unwrap();
        assert_eq!(
            ts.child_command,
            Some(ChildCommand::Legacy("btop".to_string()))
        );
        assert_eq!(ts.child_cwd, Some("/home/user".to_string()));
    }

    #[test]
    fn saved_window_deserializes_v3_array_child_command() {
        let json = r#"{
            "id": 42,
            "app_id": "kitty",
            "is_focused": true,
            "pid": 1234,
            "terminal_state": {
                "child_command": ["nvim", "/path/to/file"],
                "child_cwd": "/home/user"
            }
        }"#;
        let w: SavedWindow = serde_json::from_str(json).unwrap();
        let ts = w.terminal_state.unwrap();
        assert_eq!(
            ts.child_command,
            Some(ChildCommand::Args(vec![
                "nvim".to_string(),
                "/path/to/file".to_string()
            ]))
        );
    }

    #[test]
    fn saved_window_deserializes_minimal() {
        let json = r#"{"id": 1, "app_id": "firefox", "is_focused": false}"#;
        let w: SavedWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.app_id, "firefox");
        assert_eq!(w.workspace_idx, None);
        assert!(w.terminal_state.is_none());
        assert!(w.pid.is_none());
    }

    #[test]
    fn saved_window_detects_legacy_workspace_id() {
        let json = r#"{
            "id": 42,
            "app_id": "kitty",
            "workspace_id": 3,
            "is_focused": true
        }"#;
        let w: SavedWindow = serde_json::from_str(json).unwrap();
        assert!(w.workspace_id.is_some());
        assert!(w.workspace_idx.is_none());
    }

    #[test]
    fn config_default_values() {
        let c = TerminalStateConfig::default();
        assert!(c.enabled);
        assert!(c.terminal_app_ids.contains(&"kitty".to_string()));
        assert!(c.shell_names.contains(&"fish".to_string()));
        assert!(c.helper_names.contains(&"kitten".to_string()));
        assert_eq!(c.max_walk_depth, 20);
    }

    #[test]
    fn session_data_parses_versioned_format() {
        let json = r#"{
            "version": 2,
            "windows": [
                {"id": 1, "app_id": "kitty", "is_focused": false}
            ]
        }"#;
        let session: SessionData = serde_json::from_str(json).unwrap();
        assert!(!session.is_legacy());
        let windows = session.into_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].app_id, "kitty");
    }

    #[test]
    fn session_data_parses_legacy_array_format() {
        let json = r#"[
            {"id": 1, "app_id": "kitty", "is_focused": false},
            {"id": 2, "app_id": "firefox", "is_focused": true}
        ]"#;
        let session: SessionData = serde_json::from_str(json).unwrap();
        assert!(session.is_legacy());
        let windows = session.into_windows();
        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn versioned_session_serializes_correctly() {
        let session = VersionedSession {
            version: SESSION_FORMAT_VERSION,
            windows: vec![SavedWindow {
                id: 42,
                app_id: "kitty".to_string(),
                workspace_idx: Some(1),
                workspace_name: None,
                workspace_output: None,
                is_focused: false,
                pid: Some(1234),
                terminal_state: Some(TerminalState {
                    child_command: Some(ChildCommand::Args(vec!["btop".to_string()])),
                    child_cwd: Some("/home/user".to_string()),
                }),
                workspace_id: None,
            }],
        };
        let json = serde_json::to_string_pretty(&session).unwrap();
        assert!(json.contains("\"version\": 3"));
        assert!(json.contains("\"windows\""));
        let parsed: SessionData = serde_json::from_str(&json).unwrap();
        assert!(!parsed.is_legacy());
    }

    #[test]
    fn get_restore_shell_returns_non_empty() {
        let shell = get_restore_shell();
        assert!(!shell.is_empty());
        assert!(shell.contains('/') || shell == "/bin/sh");
    }

    #[test]
    fn get_restore_shell_prefers_env_var() {
        let shell = get_restore_shell();
        if let Ok(env_shell) = std::env::var("SHELL")
            && !env_shell.is_empty()
        {
            assert_eq!(shell, env_shell);
        }
    }
}
