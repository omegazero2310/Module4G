use super::*;

#[cfg(windows)]
#[tauri::command]
pub(super) async fn execute_at(command: String) -> Result<Vec<String>, String> {
    let command = validate_console(&command, false).map_err(|e| e.to_string())?;
    let response = request_line(&command).await?;
    Ok(response.lines().map(str::to_owned).collect())
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn execute_at(_command: String) -> Result<Vec<String>, String> {
    Err("AT execution is available only on Windows.".into())
}
