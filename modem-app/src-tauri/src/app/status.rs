use super::*;

#[cfg(windows)]
#[tauri::command]
pub(super) async fn get_status() -> Result<Status, String> {
    let response = request_line("STATUS").await?;
    let status = parse_status_response(&response)?;
    log_status_change(&status);
    Ok(status)
}

pub(super) fn parse_status_response(response: &str) -> Result<Status, String> {
    // Preserve a trailing empty delivery-tracking error field. `trim_end()`
    // would remove its tab as whitespace and turn a valid nine-field response
    // into eight fields. Seven fields are accepted for older services.
    let fields: Vec<_> = response
        .trim_end_matches(['\r', '\n'])
        .split('\t')
        .collect();
    if !matches!(fields.len(), 7 | 9) || fields[0] != "STATUS" {
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
        delivery_tracking_available: fields.get(7).is_some_and(|value| *value == "true"),
        delivery_tracking_error: fields.get(8).copied().unwrap_or_default().into(),
    })
}

#[cfg(windows)]
pub(super) fn log_status_change(status: &Status) {
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
pub(super) async fn get_status() -> Result<Status, String> {
    Err("The modem service is available only on Windows.".into())
}
