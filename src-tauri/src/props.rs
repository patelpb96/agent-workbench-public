//! Commands behind the Properties window: read and save the user settings file. The webview never
//! supplies a path; the file is always `settings.json` in the app data folder. See `settings.rs`.
use crate::{error, AppError};
use agent_workbench::settings::{self, Settings};
use std::path::{Path, PathBuf};
use tauri::Manager;

pub fn settings_path(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    app.path().app_data_dir().map(|d| d.join("settings.json")).map_err(|_| error("settings_failed", "The app data folder is unavailable."))
}

/// The current settings (defaults when none are saved).
#[tauri::command]
pub fn settings_load(app: tauri::AppHandle) -> Result<Settings, AppError> {
    Ok(settings::load(&settings_path(&app)?))
}

/// Validate and save. A folder must exist when it is saved (a deleted one later is tolerated when used).
#[tauri::command]
pub fn settings_save(app: tauri::AppHandle, settings: Settings) -> Result<Settings, AppError> {
    if !settings.is_valid() {
        return Err(error("settings_invalid", "A folder must be an absolute path such as C:\\Users\\you\\project, without quotes or special characters."));
    }
    for (name, folder) in [("The terminal start folder", &settings.terminals.start_folder), ("The agent folder", &settings.agents.launch_folder)] {
        if !folder.is_empty() && !Path::new(folder).is_dir() {
            return Err(error("settings_invalid", format!("{name} does not exist.")));
        }
    }
    settings::save(&settings_path(&app)?, &settings).map_err(|_| error("settings_failed", "The settings could not be saved."))?;
    Ok(settings)
}

/// The standard Windows folder picker. `start` (if it is a valid existing folder) is where the dialog opens.
/// Returns the chosen path, or `None` when the user cancels. The result is only text for a field; whatever uses
/// it validates it again.
#[tauri::command]
pub async fn pick_folder(start: Option<String>) -> Result<Option<String>, AppError> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose a folder");
    if let Some(dir) = start.as_deref().map(str::trim).filter(|d| wb_session::proto::valid_start_dir(d) && Path::new(d).is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    Ok(dialog.pick_folder().await.map(|handle| handle.path().display().to_string()))
}

fn roots(app: &tauri::AppHandle) -> Result<agent_workbench::locations::Roots, AppError> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    Ok(agent_workbench::locations::Roots {
        app_data: app.path().app_data_dir().map_err(|_| error("locations_failed", "The app data folder is unavailable."))?,
        local_data: var("LOCALAPPDATA").ok_or_else(|| error("locations_failed", "The local data folder is unavailable."))?,
        home: var("USERPROFILE").ok_or_else(|| error("locations_failed", "The home folder is unavailable."))?,
        exe_dir: std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).ok_or_else(|| error("locations_failed", "The app folder is unavailable."))?,
    })
}

/// Where Agent Workbench's things live on this machine (only places that exist). Display only.
#[tauri::command]
pub async fn locations_list(app: tauri::AppHandle) -> Result<Vec<agent_workbench::locations::Location>, AppError> {
    let roots = roots(&app)?;
    tauri::async_runtime::spawn_blocking(move || agent_workbench::locations::collect(&roots, std::time::SystemTime::now()))
        .await
        .map_err(|_| error("locations_failed", "The request was interrupted."))
}

/// Show one of those places in Explorer. The webview sends only the id; the path is looked up here again.
#[tauri::command]
pub async fn locations_open(app: tauri::AppHandle, id: String) -> Result<(), AppError> {
    use std::os::windows::process::CommandExt;
    let roots = roots(&app)?;
    let found = tauri::async_runtime::spawn_blocking(move || agent_workbench::locations::resolve(&roots, &id, std::time::SystemTime::now()))
        .await
        .map_err(|_| error("locations_failed", "The request was interrupted."))?
        .ok_or_else(|| error("locations_failed", "That place is not available on this machine."))?;
    let mut command = std::process::Command::new("explorer.exe");
    if found.file {
        command.arg(format!("/select,{}", found.path));
    } else {
        command.arg(&found.path);
    }
    // Explorer reports a nonzero status even when it opened the window, so only a failure to start is an error.
    command.creation_flags(0x0800_0000).spawn().map(|_| ()).map_err(|_| error("locations_failed", "Explorer could not be started."))
}

/// The Files panel: one folder's entries, read-only (`files.rs`). The path is checked here again.
#[tauri::command]
pub async fn files_list(path: String, show_hidden: bool) -> Result<agent_workbench::files::Listing, AppError> {
    use agent_workbench::files::{list, ListError};
    tauri::async_runtime::spawn_blocking(move || list(&path, show_hidden))
        .await
        .map_err(|_| error("files_failed", "The request was interrupted."))?
        .map_err(|e| match e {
            ListError::Invalid => error("files_invalid", "Use an absolute folder path such as C:\\Users\\you\\project."),
            ListError::Unreadable => error("files_failed", "That folder does not exist or cannot be read."),
        })
}

/// Where the Files panel starts when nothing else is chosen: the user's folder.
#[tauri::command]
pub fn files_home() -> Result<String, AppError> {
    std::env::var("USERPROFILE").ok().filter(|p| wb_session::proto::valid_start_dir(p)).ok_or_else(|| error("files_failed", "The home folder is unavailable."))
}

/// Show a file or folder in File Explorer (selected, for a file). Nothing is opened or run.
#[tauri::command]
pub async fn files_reveal(path: String) -> Result<(), AppError> {
    use std::os::windows::process::CommandExt;
    let path = path.trim().to_owned();
    let file = agent_workbench::files::existing(&path).ok_or_else(|| error("files_failed", "That file or folder is not available."))?;
    let mut command = std::process::Command::new("explorer.exe");
    if file {
        command.arg(format!("/select,{path}"));
    } else {
        command.arg(&path);
    }
    // Explorer reports a nonzero status even when it opened the window, so only a failure to start is an error.
    command.creation_flags(0x0800_0000).spawn().map(|_| ()).map_err(|_| error("files_failed", "Explorer could not be started."))
}
