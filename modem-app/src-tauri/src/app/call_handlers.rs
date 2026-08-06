use super::*;

#[cfg(windows)]
#[tauri::command]
pub(super) async fn make_call(
    destination: String,
    audio_id: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination =
        modemd::sms::normalize_call_destination(&destination).map_err(|e| e.to_string())?;
    request_json(serde_json::json!({
        "command":"make_call",
        "destination":destination,
        "audioId":audio_id
    }))
    .await
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn make_call(
    _destination: String,
    _audio_id: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    Err("Calls are available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn get_current_audio() -> Result<Option<UploadedAudio>, String> {
    request_json(serde_json::json!({"command":"get_current_audio"})).await
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn list_audio() -> Result<Vec<UploadedAudio>, String> {
    request_json(serde_json::json!({"command":"list_audio"})).await
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn get_call_data() -> Result<CallData, String> {
    request_json(serde_json::json!({"command":"get_call_data"})).await
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn get_call_data() -> Result<CallData, String> {
    Ok(CallData {
        calls: Vec::new(),
        audio: Vec::new(),
    })
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn list_audio() -> Result<Vec<UploadedAudio>, String> {
    Ok(Vec::new())
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn select_audio(audio_id: String) -> Result<UploadedAudio, String> {
    request_json(serde_json::json!({"command":"select_audio","audioId":audio_id})).await
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn select_audio(_audio_id: String) -> Result<UploadedAudio, String> {
    Err("Audio selection is available only on Windows.".into())
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn get_current_audio() -> Result<Option<UploadedAudio>, String> {
    Ok(None)
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn upload_audio(
    name: String,
    data: Vec<u8>,
    state: tauri::State<'_, AppState>,
) -> Result<UploadedAudio, String> {
    let settings: CoreSettings =
        request_json(serde_json::json!({"command":"get_settings"})).await?;
    *state.settings.lock().map_err(|_| "Settings lock failed")? = settings.clone();
    modemd::audio::validate_audio_name(&name).map_err(|e| e.to_string())?;
    modemd::audio::inspect_amr(&data, settings.max_audio_bytes).map_err(|e| e.to_string())?;
    request_json(serde_json::json!({"command":"upload_audio","name":name,"data":data})).await
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn upload_audio(
    _name: String,
    _data: Vec<u8>,
    _state: tauri::State<'_, AppState>,
) -> Result<UploadedAudio, String> {
    Err("Audio upload is available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    let _: serde_json::Value = request_json(serde_json::json!({"command":"hang_up"})).await?;
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    Err("Calls are available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn list_calls(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    request_json(serde_json::json!({"command":"list_calls"})).await
}

#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn list_calls(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    Ok(Vec::new())
}
