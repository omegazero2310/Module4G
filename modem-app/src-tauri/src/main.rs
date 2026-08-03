use modemd::{at::validate_console, settings::Settings as CoreSettings};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status { service_version: String, state: String, port: String, sim_state: String, registration: String, signal_rssi: i32, last_error: String }

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Port { name: String, vid: u16, pid: u16, label: String, available: bool, dedicated_at: bool }

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings { usb_vid: u16, usb_pid: u16, port_override: String, baud: u32, call_timeout_seconds: u32, upload_pacing_ms: u32, max_audio_bytes: usize, ussd_code: String, ussd_timeout_seconds: u32, currency: String, low_balance_threshold: f64, balance_regex: String }

impl From<CoreSettings> for Settings { fn from(value: CoreSettings) -> Self { Self { usb_vid: value.usb_vid, usb_pid: value.usb_pid, port_override: value.port_override.unwrap_or_default(), baud: value.baud, call_timeout_seconds: value.call_timeout_seconds, upload_pacing_ms: value.upload_pacing_ms, max_audio_bytes: value.max_audio_bytes, ussd_code: value.ussd_code, ussd_timeout_seconds: value.ussd_timeout_seconds, currency: value.currency, low_balance_threshold: value.low_balance_threshold, balance_regex: value.balance_regex.unwrap_or_default() } } }
impl From<Settings> for CoreSettings { fn from(value: Settings) -> Self { Self { usb_vid: value.usb_vid, usb_pid: value.usb_pid, port_override: (!value.port_override.trim().is_empty()).then(|| value.port_override.trim().to_owned()), baud: value.baud, call_timeout_seconds: value.call_timeout_seconds, upload_pacing_ms: value.upload_pacing_ms, max_audio_bytes: value.max_audio_bytes, ussd_code: value.ussd_code, ussd_timeout_seconds: value.ussd_timeout_seconds, currency: value.currency, low_balance_threshold: value.low_balance_threshold, balance_regex: (!value.balance_regex.is_empty()).then_some(value.balance_regex) } } }

struct AppState(Mutex<CoreSettings>);

#[cfg(windows)]
async fn request_line(request: &str) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut client = ClientOptions::new().open(r"\\.\pipe\a7670-modemd-v1").map_err(|e| format!("Cannot connect to the local modem service: {e}"))?;
    client.write_all(format!("{request}\n").as_bytes()).await.map_err(|e| format!("Cannot send request: {e}"))?;
    let mut response = String::new();
    BufReader::new(client).read_line(&mut response).await.map_err(|e| format!("Cannot read response: {e}"))?;
    Ok(response)
}

#[cfg(windows)]
#[tauri::command]
async fn get_status() -> Result<Status, String> {
    let response = request_line("STATUS").await?;
    let fields: Vec<_> = response.trim_end().split('\t').collect();
    if fields.len() != 7 || fields[0] != "STATUS" { return Err("The modem service returned an invalid status response.".into()); }
    Ok(Status { service_version: fields[1].into(), state: fields[2].into(), port: fields[3].into(), sim_state: fields[4].into(), registration: fields[5].into(), signal_rssi: fields[6].parse().map_err(|_| "Invalid signal value")?, last_error: String::new() })
}

#[cfg(not(windows))]
#[tauri::command] async fn get_status() -> Result<Status, String> { Err("The modem service is available only on Windows.".into()) }

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Result<Settings, String> { Ok(state.0.lock().map_err(|_| "Settings lock failed")?.clone().into()) }

#[tauri::command]
fn update_settings(settings: Settings, state: tauri::State<'_, AppState>) -> Result<Settings, String> {
    let core: CoreSettings = settings.into(); core.validate().map_err(|e| e.to_string())?;
    *state.0.lock().map_err(|_| "Settings lock failed")? = core.clone(); Ok(core.into())
}

#[tauri::command]
fn list_ports(state: tauri::State<'_, AppState>) -> Result<Vec<Port>, String> {
    let settings = state.0.lock().map_err(|_| "Settings lock failed")?.clone();
    Ok(modemd::hardware::enumerate(&settings).unwrap_or_default().into_iter().map(|p| { let label = p.product.unwrap_or_default(); Port { dedicated_at: label.to_ascii_lowercase().contains("at port"), name: p.name, vid: p.vid, pid: p.pid, label, available: true } }).collect())
}

#[cfg(windows)]
#[tauri::command]
async fn execute_at(command: String) -> Result<Vec<String>, String> {
    let command = validate_console(&command, false).map_err(|e| e.to_string())?;
    let response = request_line(&command).await?;
    Ok(response.lines().map(str::to_owned).collect())
}
#[cfg(not(windows))]
#[tauri::command] async fn execute_at(_command: String) -> Result<Vec<String>, String> { Err("AT execution is available only on Windows.".into()) }

fn main() {
    tauri::Builder::default().manage(AppState(Mutex::new(CoreSettings::default())))
        .invoke_handler(tauri::generate_handler![get_status, get_settings, update_settings, list_ports, execute_at])
        .run(tauri::generate_context!()).expect("failed to run modem app");
}
