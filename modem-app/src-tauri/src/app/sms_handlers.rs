use super::*;

#[cfg(windows)]
#[tauri::command]
pub(super) async fn send_sms(
    destination: String,
    body: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination =
        modemd::sms::normalize_sms_destination(&destination).map_err(|e| e.to_string())?;
    let encoding = modemd::sms::validate_body(&body).map_err(|e| e.to_string())?;
    if encoding != "GSM-7" {
        return Err("UCS2 modem transmission is not available yet; use GSM-7 text".into());
    }
    request_json(serde_json::json!({"command":"send_sms","destination":destination,"body":body}))
        .await
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn send_sms(
    _destination: String,
    _body: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    Err("SMS is available only on Windows.".into())
}

#[tauri::command]
pub(super) async fn list_sms(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"list_sms"})).await
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[tauri::command]
pub(super) async fn sync_sms() -> Result<usize, String> {
    #[cfg(windows)]
    {
        #[derive(Deserialize)]
        struct Count {
            count: usize,
        }
        Ok(
            request_json::<Count>(serde_json::json!({"command":"sync_sms"}))
                .await?
                .count,
        )
    }
    #[cfg(not(windows))]
    {
        Err("SMS synchronization is available only on Windows.".into())
    }
}
