use modemd::{at::validate_console, settings::Settings as CoreSettings};
use serde::{Deserialize, Serialize};
use std::{
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn log_event(area: &str, message: impl AsRef<str>) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "[{}.{:03}] [{area}] {}",
        elapsed.as_secs(),
        elapsed.subsec_millis(),
        message.as_ref()
    );
}

fn logged_request(request: &str) -> String {
    if request.starts_with('{') {
        let command = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|v| v.get("command").and_then(|x| x.as_str()).map(str::to_owned))
            .unwrap_or_else(|| "invalid".into());
        return format!("JSON {command} (SMS and balance content redacted)");
    } else if request.starts_with("SMS|") {
        "SMS send (destination and message redacted)".into()
    } else if request.starts_with("DIAL|") {
        "DIAL (destination redacted)".into()
    } else if request.starts_with("USSD|") {
        "USSD request (code redacted)".into()
    } else if request == "BALANCE" {
        "BALANCE check (response redacted)".into()
    } else {
        request.into()
    }
}

fn logged_response(request: &str, response: &str) -> String {
    if request.starts_with('{') {
        return if response.contains("\"ok\":true") {
            "JSON response received (SMS and balance content redacted)".into()
        } else {
            "JSON request failed (details redacted)".into()
        };
    }
    if request == "BALANCE" {
        return if response.starts_with("ERROR:") {
            "ERROR (details redacted)".into()
        } else {
            "balance response received (content redacted)".into()
        };
    }

    // Modems can echo dial strings or return caller IDs. Hide long digit runs in
    // console logs while retaining ordinary result codes, RSSI, and error numbers.
    let mut output = String::new();
    let mut digits = String::new();
    let flush_digits = |output: &mut String, digits: &mut String| {
        if digits.len() >= 7 {
            output.push_str("<redacted-number>");
        } else {
            output.push_str(digits);
        }
        digits.clear();
    };
    for character in response.trim().chars() {
        if character.is_ascii_digit() {
            digits.push(character);
        } else {
            flush_digits(&mut output, &mut digits);
            output.push(character);
        }
    }
    flush_digits(&mut output, &mut digits);
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn suppress_successful_poll_log(request: &str) -> bool {
    if request == "STATUS" {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(|command| command.as_str())
                .map(|command| matches!(command, "list_calls" | "get_current_audio" | "list_audio"))
        })
        .unwrap_or(false)
}

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

#[derive(Clone, Serialize, Deserialize)]
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
struct Record {
    id: String,
    peer: String,
    body: String,
    state: String,
    detail: String,
    created_at_ms: u64,
    answer_classification: String,
    end_reason: String,
    alerting_at_ms: u64,
    release_cause: String,
    direction: String,
    kind: String,
    source: String,
    storage: String,
    storage_index: i32,
    storage_indices: Vec<i32>,
    part_count: i32,
    parts_received: i32,
    multipart_complete: bool,
    modem_status: String,
    modem_timestamp: String,
    encoding: String,
    dcs: i32,
    length: i32,
    service_center: String,
    message_reference: String,
    delivery_status: String,
    synchronized_at_ms: i64,
    present_on_modem: bool,
    sms_id: String,
    audio_id: String,
    error: String,
    duration_seconds: u32,
    connected_at_ms: i64,
    ended_at_ms: i64,
    #[serde(skip)]
    voice_begin_seen: bool,
}

struct AppState {
    settings: Mutex<CoreSettings>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadedAudio {
    id: String,
    name: String,
    format: String,
    size: u64,
    module_path: String,
    duration_ms: u64,
    created_at_ms: i64,
    state: String,
    is_current: bool,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            id: String::new(),
            peer: String::new(),
            body: String::new(),
            state: String::new(),
            detail: String::new(),
            created_at_ms: 0,
            answer_classification: String::new(),
            end_reason: String::new(),
            alerting_at_ms: 0,
            release_cause: String::new(),
            direction: String::new(),
            kind: String::new(),
            source: String::new(),
            storage: String::new(),
            storage_index: -1,
            storage_indices: Vec::new(),
            part_count: 1,
            parts_received: 1,
            multipart_complete: true,
            modem_status: String::new(),
            modem_timestamp: String::new(),
            encoding: String::new(),
            dcs: -1,
            length: 0,
            service_center: String::new(),
            message_reference: String::new(),
            delivery_status: String::new(),
            synchronized_at_ms: 0,
            present_on_modem: false,
            sms_id: String::new(),
            audio_id: String::new(),
            error: String::new(),
            duration_seconds: 0,
            connected_at_ms: 0,
            ended_at_ms: 0,
            voice_begin_seen: false,
        }
    }
}

#[cfg(windows)]
async fn request_line(request: &str) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;
    let display_request = logged_request(request);
    let started = Instant::now();
    let suppress_success = suppress_successful_poll_log(request);
    if !suppress_success {
        log_event("APP -> SERVICE", &display_request);
    }
    let result = async {
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
        Ok::<_, String>(response)
    }
    .await;
    match &result {
        Ok(response) if !suppress_success => log_event(
            "SERVICE -> APP",
            format!(
                "{} => {} ({} ms)",
                display_request,
                logged_response(request, response),
                started.elapsed().as_millis()
            ),
        ),
        Ok(_) => {}
        Err(error) => log_event(
            "MODEM ERROR",
            format!(
                "{} => {error} ({} ms)",
                display_request,
                started.elapsed().as_millis()
            ),
        ),
    }
    result
}

#[cfg(windows)]
#[tauri::command]
async fn get_status() -> Result<Status, String> {
    let response = request_line("STATUS").await?;
    let fields: Vec<_> = response.trim_end().split('\t').collect();
    if fields.len() != 7 || fields[0] != "STATUS" {
        return Err("The modem service returned an invalid status response.".into());
    }
    let status = Status {
        service_version: fields[1].into(),
        state: fields[2].into(),
        port: fields[3].into(),
        sim_state: fields[4].into(),
        registration: fields[5].into(),
        signal_rssi: fields[6].parse().map_err(|_| "Invalid signal value")?,
        last_error: String::new(),
    };
    log_status_change(&status);
    Ok(status)
}

#[cfg(windows)]
fn log_status_change(status: &Status) {
    static LAST_STATUS: Mutex<Option<String>> = Mutex::new(None);
    let summary = format!(
        "state={} port={} sim={} registration={} signal_rssi={}",
        status.state, status.port, status.sim_state, status.registration, status.signal_rssi
    );
    if let Ok(mut previous) = LAST_STATUS.lock()
        && previous.as_ref() != Some(&summary)
    {
        log_event("MODEM STATE", &summary);
        *previous = Some(summary);
    }
}

#[cfg(not(windows))]
#[tauri::command]
async fn get_status() -> Result<Status, String> {
    Err("The modem service is available only on Windows.".into())
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, AppState>) -> Result<Settings, String> {
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
async fn update_settings(
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
fn list_ports(state: tauri::State<'_, AppState>) -> Result<Vec<Port>, String> {
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

#[cfg(windows)]
async fn request_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, String> {
    let raw = request_line(&value.to_string()).await?;
    let envelope: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| format!("Invalid service response: {e}"))?;
    if !envelope
        .get("ok")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
    {
        return Err(envelope
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("Service request failed")
            .to_owned());
    }
    serde_json::from_value(envelope.get("data").cloned().unwrap_or_default())
        .map_err(|e| format!("Invalid service data: {e}"))
}

#[cfg(windows)]
#[tauri::command]
async fn send_sms(
    destination: String,
    body: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination = modemd::sms::normalize_number(&destination).map_err(|e| e.to_string())?;
    let encoding = modemd::sms::validate_body(&body).map_err(|e| e.to_string())?;
    if encoding != "GSM-7" {
        return Err("UCS2 modem transmission is not available yet; use GSM-7 text".into());
    }
    request_json(serde_json::json!({"command":"send_sms","destination":destination,"body":body}))
        .await
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
async fn list_sms(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
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
async fn sync_sms() -> Result<usize, String> {
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

#[cfg(windows)]
#[tauri::command]
async fn make_call(
    destination: String,
    audio_id: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    let destination = modemd::sms::normalize_number(&destination).map_err(|e| e.to_string())?;
    request_json(serde_json::json!({
        "command":"make_call",
        "destination":destination,
        "audioId":audio_id
    }))
    .await
}
#[cfg(not(windows))]
#[tauri::command]
async fn make_call(
    _destination: String,
    _audio_id: String,
    _state: tauri::State<'_, AppState>,
) -> Result<Record, String> {
    Err("Calls are available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
async fn get_current_audio() -> Result<Option<UploadedAudio>, String> {
    request_json(serde_json::json!({"command":"get_current_audio"})).await
}

#[cfg(windows)]
#[tauri::command]
async fn list_audio() -> Result<Vec<UploadedAudio>, String> {
    request_json(serde_json::json!({"command":"list_audio"})).await
}
#[cfg(not(windows))]
#[tauri::command]
async fn list_audio() -> Result<Vec<UploadedAudio>, String> {
    Ok(Vec::new())
}

#[cfg(windows)]
#[tauri::command]
async fn select_audio(audio_id: String) -> Result<UploadedAudio, String> {
    request_json(serde_json::json!({"command":"select_audio","audioId":audio_id})).await
}
#[cfg(not(windows))]
#[tauri::command]
async fn select_audio(_audio_id: String) -> Result<UploadedAudio, String> {
    Err("Audio selection is available only on Windows.".into())
}
#[cfg(not(windows))]
#[tauri::command]
async fn get_current_audio() -> Result<Option<UploadedAudio>, String> {
    Ok(None)
}

#[cfg(windows)]
#[tauri::command]
async fn upload_audio(
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
async fn upload_audio(
    _name: String,
    _data: Vec<u8>,
    _state: tauri::State<'_, AppState>,
) -> Result<UploadedAudio, String> {
    Err("Audio upload is available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    let _: serde_json::Value = request_json(serde_json::json!({"command":"hang_up"})).await?;
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    Err("Calls are available only on Windows.".into())
}

#[cfg(windows)]
#[tauri::command]
async fn list_calls(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    request_json(serde_json::json!({"command":"list_calls"})).await
}

#[cfg(not(windows))]
#[tauri::command]
async fn list_calls(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_logging_redacts_private_request_data() {
        assert_eq!(
            logged_request("SMS|+66812345678|48656C6C6F"),
            "SMS send (destination and message redacted)"
        );
        assert_eq!(
            logged_request("DIAL|+66812345678"),
            "DIAL (destination redacted)"
        );
        assert_eq!(
            logged_response("AT+CLCC", "+CLCC: 1,0,0,0,0,\"+66812345678\",145\r\nOK\n"),
            "+CLCC: 1,0,0,0,0,\"+<redacted-number>\",145 | OK"
        );
        assert_eq!(
            logged_response("BALANCE", "Your balance is 100000 VND"),
            "balance response received (content redacted)"
        );
        assert_eq!(
            logged_response(
                "{\"command\":\"list_sms\"}",
                "{\"ok\":true,\"data\":[{\"body\":\"secret\"}]}"
            ),
            "JSON response received (SMS and balance content redacted)"
        );
    }

    #[test]
    fn successful_high_frequency_polls_are_suppressed() {
        assert!(suppress_successful_poll_log("STATUS"));
        assert!(suppress_successful_poll_log(r#"{"command":"list_calls"}"#));
        assert!(suppress_successful_poll_log(
            r#"{"command":"get_current_audio"}"#
        ));
        assert!(!suppress_successful_poll_log(
            r#"{"command":"upload_audio"}"#
        ));
    }
}

#[cfg(windows)]
#[tauri::command]
async fn check_balance(_state: tauri::State<'_, AppState>) -> Result<Record, String> {
    let wire: BalanceWire = request_json(serde_json::json!({"command":"check_balance"})).await?;
    Ok(wire.into())
}
#[cfg(not(windows))]
#[tauri::command]
async fn check_balance(_state: tauri::State<'_, AppState>) -> Result<Record, String> {
    Err("Balance checks are available only on Windows.".into())
}
#[tauri::command]
async fn list_balance_checks(_state: tauri::State<'_, AppState>) -> Result<Vec<Record>, String> {
    #[cfg(windows)]
    {
        Ok(
            request_json::<Vec<BalanceWire>>(serde_json::json!({"command":"list_balances"}))
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BalanceWire {
    id: String,
    raw: String,
    created_at_ms: i64,
    sms_id: String,
}
impl From<BalanceWire> for Record {
    fn from(x: BalanceWire) -> Self {
        Record {
            id: x.id,
            peer: "191".into(),
            body: x.raw.clone(),
            detail: x.raw,
            state: "received".into(),
            created_at_ms: x.created_at_ms as u64,
            sms_id: x.sms_id,
            ..Default::default()
        }
    }
}

fn main() {
    log_event("APP", "A7670 Modem application starting");
    tauri::Builder::default()
        .manage(AppState {
            settings: Mutex::new(CoreSettings::default()),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_settings,
            update_settings,
            list_ports,
            execute_at,
            send_sms,
            sync_sms,
            list_sms,
            get_current_audio,
            list_audio,
            select_audio,
            upload_audio,
            make_call,
            hang_up,
            list_calls,
            check_balance,
            list_balance_checks
        ])
        .run(tauri::generate_context!())
        .expect("failed to run modem app");
}
