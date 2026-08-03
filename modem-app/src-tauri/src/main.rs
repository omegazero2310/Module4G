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
    if request.starts_with("SMS|") {
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
    answer_classification: String,
    end_reason: String,
    alerting_at_ms: u64,
    release_cause: String,
    #[serde(skip)]
    voice_begin_seen: bool,
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
    let display_request = logged_request(request);
    let started = Instant::now();
    log_event("APP -> MODEM", &display_request);
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
        Ok(response) => log_event(
            "MODEM -> APP",
            format!(
                "{} => {} ({} ms)",
                display_request,
                logged_response(request, response),
                started.elapsed().as_millis()
            ),
        ),
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
    log_event("APP", "Settings opened");
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
    log_event("APP", "Settings save requested (values not logged)");
    let core: CoreSettings = settings.into();
    core.validate().map_err(|e| e.to_string())?;
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
        answer_classification: String::new(),
        end_reason: String::new(),
        alerting_at_ms: 0,
        release_cause: String::new(),
        voice_begin_seen: false,
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
        answer_classification: "unknown".into(),
        end_reason: "none".into(),
        alerting_at_ms: 0,
        release_cause: String::new(),
        voice_begin_seen: false,
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
        call.end_reason = "Local hang-up".into();
        call.answer_classification = if call.voice_begin_seen {
            "Answered".into()
        } else {
            "Unknown".into()
        };
    }
    Ok(())
}
#[cfg(not(windows))]
#[tauri::command]
async fn hang_up(_state: tauri::State<'_, AppState>) -> Result<(), String> {
    Err("Calls are available only on Windows.".into())
}
fn is_live_call(state: &str) -> bool {
    matches!(state, "dialing" | "ringing" | "active")
}

fn end_reason_label(reason: modemd::call::EndReason) -> &'static str {
    use modemd::call::EndReason::*;
    match reason {
        None => "None",
        LocalHangUp => "Local hang-up",
        RemoteHangUp => "Remote hang-up",
        Busy => "Busy",
        NoAnswer => "No answer",
        Unreachable => "Unreachable / cannot connect",
        NetworkError => "Network error",
        SignalingTimeout => "Signaling timeout",
        ModemLost => "Modem lost",
        CallError => "Call error",
    }
}

/// Applies all events from one poll as a batch. Explicit terminal result codes
/// take precedence even when a generic END/NO CARRIER occurs in the same read.
fn apply_call_response(call: &mut Record, response: &str) -> bool {
    let lines: Vec<_> = response.split(" | ").map(str::trim).collect();
    let explicit = if lines.contains(&"BUSY") {
        Some(("Busy", "Busy"))
    } else if lines.contains(&"NO ANSWER") {
        Some(("No answer", "No answer"))
    } else {
        None
    };
    let mut needs_ceer = false;
    for line in lines {
        if line == "VOICE CALL: BEGIN" || line == "VOICE CALL:BEGIN" {
            call.voice_begin_seen = true;
            call.answer_classification = "Answered".into();
            call.state = "active".into();
        } else if line.starts_with("VOICE CALL: END") || line.starts_with("VOICE CALL:END") {
            needs_ceer = true;
            call.state = "ended".into();
            if !call.voice_begin_seen {
                call.answer_classification = "Not answered".into();
                call.end_reason = "No answer".into();
            } else {
                call.end_reason = "Remote hang-up".into();
            }
        } else if line == "NO CARRIER" {
            needs_ceer = true;
            call.state = "ended".into();
        } else if let Some(event) = modemd::call::parse_urc(line)
            && let modemd::call::CallUrc::Clcc {
                direction: 0,
                state,
            } = event
        {
            call.state = match state {
                2 => "dialing",
                3 => "ringing",
                0 | 4 | 5 => "active",
                _ => call.state.as_str(),
            }
            .into();
        }
    }
    if let Some((state, reason)) = explicit {
        call.state = "ended".into();
        call.answer_classification = "Not answered".into();
        call.end_reason = reason.into();
        call.detail = state.into();
    } else {
        call.detail = response.into();
    }
    needs_ceer
}

fn apply_release_cause(call: &mut Record, response: &str) {
    let cause = response
        .split(" | ")
        .find(|line| line.trim().starts_with("+CEER:"))
        .unwrap_or(response)
        .trim()
        .trim_start_matches("+CEER:")
        .trim();
    let cause = modemd::call::sanitize_cause(cause);
    call.release_cause = cause.clone();
    if matches!(
        call.end_reason.as_str(),
        "Busy" | "No answer" | "Local hang-up"
    ) {
        return;
    }
    let reason = modemd::call::classify_ceer(&cause, call.voice_begin_seen);
    call.end_reason = end_reason_label(reason).into();
    call.answer_classification = if call.voice_begin_seen {
        "Answered".into()
    } else {
        "Not answered".into()
    };
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
        let settings = state
            .settings
            .lock()
            .map_err(|_| "Settings lock failed")?
            .clone();
        let current_ms = now_ms();
        let (needs_ceer, overall_timeout) = {
            let mut needs_ceer = false;
            let mut overall_timeout = false;
            let mut calls = state.calls.lock().map_err(|_| "Call history lock failed")?;
            if let Some(call) = calls.iter_mut().find(|call| is_live_call(&call.state)) {
                needs_ceer = apply_call_response(call, &response);
                if is_live_call(&call.state)
                    && current_ms.saturating_sub(call.created_at_ms)
                        >= u64::from(settings.call_timeout_seconds) * 1_000
                {
                    call.state = "ended".into();
                    call.detail =
                        "No terminal call signaling was received before the safety timeout".into();
                    call.answer_classification = "Unknown".into();
                    call.end_reason = "Signaling timeout".into();
                    overall_timeout = true;
                }
            }
            (needs_ceer, overall_timeout)
        };
        if needs_ceer {
            let cause = ensure_ok(request_line("CALLCAUSE").await?)?;
            let mut calls = state.calls.lock().map_err(|_| "Call history lock failed")?;
            if let Some(call) = calls.first_mut().filter(|call| call.state == "ended") {
                apply_release_cause(call, &cause);
            }
        }
        if overall_timeout {
            ensure_ok(request_line("HANGUP").await?)?;
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
        let mut call = test_call();
        assert!(!apply_call_response(&mut call, "+CLCC: 1,0,3,0,0"));
        assert_eq!(call.state, "ringing");
        apply_call_response(&mut call, "+CLCC: 1,0,0,0,0");
        assert_eq!(call.answer_classification, "unknown");
        assert!(apply_call_response(
            &mut call,
            "VOICE CALL: BEGIN | VOICE CALL: END | NO CARRIER"
        ));
        assert_eq!(call.answer_classification, "Answered");
        assert_eq!(call.end_reason, "Remote hang-up");
    }

    fn test_call() -> Record {
        Record {
            id: "1".into(),
            peer: String::new(),
            body: String::new(),
            state: "dialing".into(),
            detail: String::new(),
            created_at_ms: 0,
            answer_classification: "unknown".into(),
            end_reason: "none".into(),
            alerting_at_ms: 0,
            release_cause: String::new(),
            voice_begin_seen: false,
        }
    }

    #[test]
    fn explicit_result_overrides_generic_terminal_in_same_batch() {
        let mut call = test_call();
        assert!(apply_call_response(
            &mut call,
            "VOICE CALL: END | NO CARRIER | BUSY"
        ));
        assert_eq!(call.answer_classification, "Not answered");
        assert_eq!(call.end_reason, "Busy");
        apply_release_cause(&mut call, "+CEER: 16 Normal call clearing");
        assert_eq!(call.end_reason, "Busy");
    }

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
    }
}

#[cfg(windows)]
#[tauri::command]
async fn check_balance(state: tauri::State<'_, AppState>) -> Result<Record, String> {
    let raw = ensure_ok(request_line("BALANCE").await?)?;
    let record = Record {
        id: id(),
        peer: "191".into(),
        body: raw.clone(),
        state: if raw.is_empty() {
            "requested".into()
        } else {
            "received".into()
        },
        detail: raw,
        created_at_ms: now_ms(),
        answer_classification: String::new(),
        end_reason: String::new(),
        alerting_at_ms: 0,
        release_cause: String::new(),
        voice_begin_seen: false,
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
    log_event("APP", "A7670 Modem application starting");
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
