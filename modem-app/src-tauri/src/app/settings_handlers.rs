use super::*;

#[tauri::command]
pub(super) async fn get_settings(state: tauri::State<'_, AppState>) -> Result<Settings, String> {
    log_event("APP", "Settings opened");
    #[cfg(windows)]
    {
        let core: CoreSettings =
            request_json(serde_json::json!({"command":"get_settings"})).await?;
        *state.settings.lock().map_err(|_| "Settings lock failed")? = core.clone();
        Ok(core.into())
    }
    #[cfg(not(windows))]
    {
        Ok(state
            .settings
            .lock()
            .map_err(|_| "Settings lock failed")?
            .clone()
            .into())
    }
}

#[tauri::command]
pub(super) async fn update_settings(
    settings: Settings,
    state: tauri::State<'_, AppState>,
) -> Result<Settings, String> {
    log_event("APP", "Settings save requested (values not logged)");
    let core: CoreSettings = settings.into();
    core.validate().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let core: CoreSettings = request_json(serde_json::json!({
        "command":"update_settings",
        "settings":core
    }))
    .await?;
    *state.settings.lock().map_err(|_| "Settings lock failed")? = core.clone();
    log_event("APP", "Settings validated and applied");
    Ok(core.into())
}

#[tauri::command]
pub(super) fn list_ports(state: tauri::State<'_, AppState>) -> Result<Vec<Port>, String> {
    log_event("APP", "COM port scan started");
    let settings = state
        .settings
        .lock()
        .map_err(|_| "Settings lock failed")?
        .clone();
    let ports: Vec<_> = modemd::hardware::enumerate(&settings)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let label = p.product.unwrap_or_default();
            Port {
                dedicated_at: label.to_ascii_lowercase().contains("at port"),
                name: p.name,
                vid: p.vid,
                pid: p.pid,
                label,
                available: true,
            }
        })
        .collect();
    log_event(
        "APP",
        format!("COM port scan completed: {} candidate(s)", ports.len()),
    );
    Ok(ports)
}
