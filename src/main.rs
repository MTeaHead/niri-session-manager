use anyhow::{Context, Result};
use niri_ipc::{Action, Reply, Request, Response, Window, WorkspaceReferenceArg, socket::Socket};
use std::{fs, path::PathBuf, sync::Arc};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
    spawn,
    sync::Notify,
    time::Duration,
    time::sleep,
};

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

/// Save the session to a file
async fn save_session(file_path: &PathBuf) -> Result<()> {
    let windows = get_niri_windows().await?;
    let json_data =
        serde_json::to_string_pretty(&windows).context("Failed to serialize window data")?;
    fs::write(file_path, json_data).context("Failed to write session file")?;
    println!("Session saved to {}", file_path.display());
    Ok(())
}

/// Restore saved session and clean it up
async fn restore_session(file_path: &PathBuf) -> Result<()> {
    if !file_path.exists() {
        println!("No previous session found at {}", file_path.display());
        return Ok(());
    }

    let session_data = fs::read_to_string(file_path).context("Failed to read session file")?;
    if session_data.trim().is_empty() {
        println!("Session file at {} is empty", file_path.display());
        return Ok(());
    }
    let windows: Vec<Window> =
        serde_json::from_str(&session_data).context("Failed to parse session JSON")?;

    let current_windows = get_niri_windows().await?;
    let mut handles = Vec::new();

    for window in windows {
        if let Some(app_id) = &window.app_id {
            if current_windows
                .iter()
                .any(|w| w.app_id == Some(app_id.clone()))
            {
                continue;
            }
        }

        let app_id = window.app_id.clone().unwrap_or_default();
        let workspace_id = window.workspace_id;

        let handle = spawn(async move {
            let spawn_socket = Socket::connect().context("Failed to connect to Niri IPC socket")?;
            let (reply, _) = spawn_socket
                .send(Request::Action(Action::Spawn {
                    command: vec![app_id.clone()],
                }))
                .context("Failed to send spawn request")?;

            if let Reply::Ok(Response::Handled) = reply {
                for _ in 0..10 {
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
                println!("Failed to spawn app: {}", app_id);
            }

            Result::<()>::Ok(())
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete.
    for handle in handles {
        handle.await.context("Task execution failed")??;
    }

    println!("Session restored.");
    // Clean up the session file after restoring.
    fs::remove_file(file_path).context("Failed to delete session file")?;
    println!("Session file cleaned up.");
    Ok(())
}

/// Handle shutdown signals and notify the main function.
async fn handle_shutdown_signals(shutdown_signal: Arc<Notify>) {
    let mut term_signal = signal(SignalKind::terminate()).expect("Failed to listen for SIGTERM");
    let mut int_signal = signal(SignalKind::interrupt()).expect("Failed to listen for SIGINT");
    let mut quit_signal = signal(SignalKind::quit()).expect("Failed to listen for SIGQUIT");

    select! {
        _ = term_signal.recv() => shutdown_signal.notify_one(),
        _ = int_signal.recv() => shutdown_signal.notify_one(),
        _ = quit_signal.recv() => shutdown_signal.notify_one(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let session_file_path = get_session_file_path()?;
    let shutdown_signal = Arc::new(Notify::new());

    restore_session(&session_file_path).await?;
    let shutdown_signal_clone = Arc::clone(&shutdown_signal);
    handle_shutdown_signals(shutdown_signal_clone).await;

    // Wait for shutdown signal
    shutdown_signal.notified().await;
    save_session(&session_file_path).await?;
    Ok(())
}
