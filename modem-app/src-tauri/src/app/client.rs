#[cfg(windows)]
use std::{sync::OnceLock, time::Instant};

#[cfg(windows)]
use super::logging::{log_event, logged_request, logged_response, suppress_successful_poll_log};

#[cfg(windows)]
static PIPE_REQUEST_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[cfg(windows)]
pub(super) async fn request_line(request: &str) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let display_request = logged_request(request);
    let started = Instant::now();
    let suppress_success = suppress_successful_poll_log(request);
    let _request_guard = PIPE_REQUEST_GATE
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
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
pub(super) async fn request_json<T: serde::de::DeserializeOwned>(
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
