use modemd::{at::validate_console, settings::Settings as CoreSettings};
use serde::{Deserialize, Serialize};
use std::{
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    service_version: String,
    state: String,
    port: String,
    sim_state: String,
    registration: String,
    signal_rssi: i32,
    last_error: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Port {
    name: String,
    vid: u16,
    pid: u16,
    label: String,
    available: bool,
    dedicated_at: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    usb_vid: u16,
    usb_pid: u16,
    port_override: String,
    baud: u32,
    call_timeout_seconds: u32,
    upload_pacing_ms: u32,
    max_audio_bytes: usize,
    ussd_code: String,
    ussd_timeout_seconds: u32,
    currency: String,
    low_balance_threshold: f64,
    balance_regex: String,
}

impl From<CoreSettings> for Settings {
    fn from(value: CoreSettings) -> Self {
        Self {
            usb_vid: value.usb_vid,
            usb_pid: value.usb_pid,
            port_override: value.port_override.unwrap_or_default(),
            baud: value.baud,
            call_timeout_seconds: value.call_timeout_seconds,
            upload_pacing_ms: value.upload_pacing_ms,
            max_audio_bytes: value.max_audio_bytes,
            ussd_code: value.ussd_code,
            ussd_timeout_seconds: value.ussd_timeout_seconds,
            currency: value.currency,
            low_balance_threshold: value.low_balance_threshold,
            balance_regex: value.balance_regex.unwrap_or_default(),
        }
    }
}
impl From<Settings> for CoreSettings {
    fn from(value: Settings) -> Self {
        Self {
            usb_vid: value.usb_vid,
            usb_pid: value.usb_pid,
            port_override: (!value.port_override.trim().is_empty())
                .then(|| value.port_override.trim().to_owned()),
            baud: value.baud,
            call_timeout_seconds: value.call_timeout_seconds,
            upload_pacing_ms: value.upload_pacing_ms,
            max_audio_bytes: value.max_audio_bytes,
            ussd_code: value.ussd_code,
            ussd_timeout_seconds: value.ussd_timeout_seconds,
            currency: value.currency,
            low_balance_threshold: value.low_balance_threshold,
            balance_regex: (!value.balance_regex.is_empty()).then_some(value.balance_regex),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    peer: String,
    body: String,
    state: String,
    detail: String,
    created_at_ms: u64,
}

struct AppState {
    settings: Mutex<CoreSettings>,
    sms: Mutex<Vec<Record>>,
    calls: Mutex<Vec<Record>>,
    balances: Mutex<Vec<Record>>,
}

#[cfg(windows)]
async fn request_line(request: &str) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut client = ClientOptions::new()
        .open(r"\\.\pipe\a7670-modemd-v1")
        .map_err(|e| format!("Cannot connect to the local modem service: {e}"))?;
    client
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(|e| format!("Cannot send request: {e}"))?;
    let mut response = String::new();
    BufReader::new(client)
        .read_line(&mut response)
        .await
        .map_err(|e| format!("Cannot read response: {e}"))?;
    Ok(response)
}

#[cfg(windows)]
#[tauri::command]
async fn get_status() -> Result<Status, String> {
    let response = request_line("STATUS").await?;
    let fields: Vec<_> = response.trim_end().split('\t').collect();
    if fields.len() != 7 || fields[0] != "STATUS" {
        return Err("The modem service returned an invalid status response.".into());
    }
    Ok(Status {
        service_version: fields[1].into(),
        state: fields[2].into(),
        port: fields[3].into(),
        sim_state: fields[4].into(),
        registration: fields[5].into(),
        signal_rssi: fields[6].parse().map_err(|_| "Invalid signal value")?,
        last_error: String::new(),
    })
}

#[cfg(not(windows))]
#[tauri::command]
async fn get_status() -> Result<Status, String> {
    Err("The modem service is available only on Windows.".into())
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Result<Settings, String> {
    Ok(state
        .settings
        .lock()
        .map_err(|_| "Settings lock failed")?
        .clone()
        .into())
}

#[tauri::command]
fn update_settings(
    settings: Settings,
    state: tauri::State<'_, AppState>,
) -> Result<Settings, String> {
    let core: CoreSettings = settings.into();
    core.validate().map_err(|e| e.to_string())?;
    *state.settings.lock().map_err(|_| "Settings lock failed")? = core.clone();
    Ok(core.into())
}

#[tauri::command]
fn list_ports(state: tauri::State<'_, AppState>) -> Result<Vec<Port>, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "Settings lock failed")?
        .clone();
    Ok(modemd::hardware::enumerate(&settings)
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
        .collect())
}

#[cfg(windows)]
#[tauri::command]
async fn execute_at(command: String) -> Result<Vec<String>, String> {
    let command = validate_console(&command, false).map_err(|e| e.to_string())?;
    let response = request_line(&command).await?;
    Ok(response.lines().map(str::to_owned).collect())
}
#[cfg(not(windows))]
#[tauri::command]
async fn execute_at(_command: String) -> Result<Vec<String>, String> {
    Err("AT execution is available only on Windows.".into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn id() -> String {
    format!("{:x}", now_ms())
}
fn ensure_ok(response: String) -> Result<String, String> {
    if response.starts_with("ERROR:") || response.trim() == "ERROR" {
        Err(response.trim().into())
    } else {
        Ok(response.trim().into())
    }
}

#[cfg(windows)]
#[tauri::command]
async fn send_sms(
    destination: String,
    body: String,
    state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination = modemd::sms::normalize_number(&destination).map_err(|e| e.to_string())?;
    let encoding = modemd::sms::validate_body(&body).map_err(|e| e.to_string())?;
    if encoding != "GSM-7" {
        return Err("UCS2 modem transmission is not available yet; use GSM-7 text".into());
    }
    let payload: String = body
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    let detail = ensure_ok(request_line(&format!("SMS|{destination}|{payload}")).await?)?;
    let record = Record {
        id: id(),
        peer: destination,
        body,
        state: "sent".into(),
        detail,
        created_at_ms: now_ms(),
    };
    state
        .sms
        .lock()
        .map_err(|_| "SMS history lock failed")?
        .insert(0, record.clone());
    Ok(record)
}
#[cfg(not(windows))]
#[tauri::command]
async fn send_sms(
    _destination: String,
    _body: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    Err("SMS is available only on Windows.".into())
}

#[tauri::command]
fn list_sms(state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    Ok(state
        .sms
        .lock()
        .map_err(|_| "SMS history lock failed")?
        .clone())
}

#[cfg(windows)]
#[tauri::command]
async fn make_call(
    destination: String,
    state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination = modemd::sms::normalize_number(&destination).map_err(|e| e.to_string())?;
    let detail = ensure_ok(request_line(&format!("DIAL|{destination}")).await?)?;
    let record = Record {
        id: id(),
        peer: destination,
        body: String::new(),
        state: "dialing".into(),
        detail,
        created_at_ms: now_ms(),
    };
    state
        .calls
        .lock()
        .map_err(|_| "Call history lock failed")?
        .insert(0, record.clone());
    Ok(record)
}
#[cfg(not(windows))]
#[tauri::command]
async fn make_call(
    _destination: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    Err("Calls are available only on Windows.".into())
}
#[cfg(windows)]
#[tauri::command]
async fn hang_up(state: tauri::State<'_, AppState>) -> Result<(), String> {
    ensure_ok(request_line("HANGUP").await?)?;
    if let Some(call) = state
        .calls
        .lock()
        .map_err(|_| "Call history lock failed")?
        .iter_mut()
        .find(|call| is_live_call(&call.state))
    {
        call.state = "ended".into();
        call.detail = "Local hang-up".into();
    }
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    Err("Calls are available only on Windows.".into())
}
fn is_live_call(state: &str) -> bool {
    matches!(state, "dialing" | "ringing" | "connected")
}

fn call_state(response: &str) -> Option<&'static str> {
    if response.contains("NO ANSWER") {
        return Some("no answer");
    }
    if response.contains("BUSY") {
        return Some("busy");
    }
    if response.contains("NO CARRIER") {
        return Some("ended");
    }
    let line = response
        .split(" | ")
        .find(|line| line.starts_with("+CLCC:"))?;
    match line.split(',').nth(2)?.trim() {
        "0" => Some("connected"),
        "2" => Some("dialing"),
        "3" => Some("ringing"),
        "4" | "5" => Some("connected"),
        "6" => Some("ended"),
        _ => None,
    }
}

#[cfg(windows)]
#[tauri::command]
async fn list_calls(state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    let has_live_call = state
        .calls
        .lock()
        .map_err(|_| "Call history lock failed")?
        .iter()
        .any(|call| is_live_call(&call.state));
    if has_live_call {
        let response = ensure_ok(request_line("CALLSTATUS").await?)?;
        let mut calls = state.calls.lock().map_err(|_| "Call history lock failed")?;
        if let Some(call) = calls.iter_mut().find(|call| is_live_call(&call.state)) {
            if let Some(next) = call_state(&response) {
                call.state = next.into();
                call.detail = response;
            } else if now_ms().saturating_sub(call.created_at_ms) >= 2_000 {
                call.state = "ended".into();
                call.detail = "Call is no longer reported by the modem".into();
            }
        }
    }
    Ok(state
        .calls
        .lock()
        .map_err(|_| "Call history lock failed")?
        .clone())
}

#[cfg(not(windows))]
#[tauri::command]
async fn list_calls(state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    Ok(state
        .calls
        .lock()
        .map_err(|_| "Call history lock failed")?
        .clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_call_progress_and_terminal_results() {
        assert_eq!(call_state("+CLCC: 1,0,3,0,0"), Some("ringing"));
        assert_eq!(call_state("+CLCC: 1,0,0,0,0"), Some("connected"));
        assert_eq!(call_state("NO ANSWER"), Some("no answer"));
        assert_eq!(call_state("BUSY"), Some("busy"));
        assert_eq!(call_state(""), None);
    }
}

#[cfg(windows)]
#[tauri::command]
async fn check_balance(state: tauri::State<'_, AppState>) -> Result<Record, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "Settings lock failed")?
        .clone();
    let raw = ensure_ok(request_line(&format!("USSD|{}", settings.ussd_code)).await?)?;
    let record = Record {
        id: id(),
        peer: settings.currency,
        body: raw.clone(),
        state: if raw.is_empty() {
            "requested".into()
        } else {
            "received".into()
        },
        detail: raw,
        created_at_ms: now_ms(),
    };
    state
        .balances
        .lock()
        .map_err(|_| "Balance history lock failed")?
        .insert(0, record.clone());
    Ok(record)
}
#[cfg(not(windows))]
#[tauri::command]
async fn check_balance(_state: tauri::State<'_, AppState>) -> Result<Record, String> {
    Err("Balance checks are available only on Windows.".into())
}
#[tauri::command]
fn list_balance_checks(state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    Ok(state
        .balances
        .lock()
        .map_err(|_| "Balance history lock failed")?
        .clone())
}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            settings: Mutex::new(CoreSettings::default()),
            sms: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            balances: Mutex::new(Vec::new()),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_settings,
            update_settings,
            list_ports,
            execute_at,
            send_sms,
            list_sms,
            make_call,
            hang_up,
            list_calls,
            check_balance,
            list_balance_checks
        ])
        .run(tauri::generate_context!())
        .expect("failed to run modem app");
}
