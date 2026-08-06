#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use super::logging::{log_event, logged_request, logged_response, suppress_successful_poll_log};

#[cfg(windows)]
const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";
#[cfg(windows)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(windows)]
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(windows)]
pub(super) async fn request_line(request: &str) -> Result<String, String> {
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ClientOptions;

    let display_request = logged_request(request);
    let started = Instant::now();
    let suppress_success = suppress_successful_poll_log(request);
    if !suppress_success {
        log_event("APP -> SERVICE", &display_request);
    }
    let result = async {
        let mut client = retry_pipe_busy(
            || ClientOptions::new().open(PIPE),
            CONNECT_TIMEOUT,
            Duration::from_millis(40),
        )
        .await
        .map_err(|error| {
            if is_pipe_busy(&error) {
                format!(
                    "The local modem service remained busy for {} seconds",
                    CONNECT_TIMEOUT.as_secs()
                )
            } else {
                format!("Cannot connect to the local modem service: {error}")
            }
        })?;
        let payload = format!("{request}\n");
        tokio::time::timeout(WRITE_TIMEOUT, client.write_all(payload.as_bytes()))
            .await
            .map_err(|_| "Timed out while sending the request to the modem service".to_owned())?
            .map_err(|error| format!("Cannot send request: {error}"))?;
        read_response(&mut BufReader::new(client), response_timeout(request)).await
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
fn is_pipe_busy(error: &std::io::Error) -> bool {
    // ERROR_PIPE_BUSY: all pipe instances are currently connected.
    error.raw_os_error() == Some(231)
}

#[cfg(windows)]
async fn retry_pipe_busy<T>(
    mut open: impl FnMut() -> std::io::Result<T>,
    deadline: Duration,
    retry_delay: Duration,
) -> std::io::Result<T> {
    let started = Instant::now();
    loop {
        match open() {
            Ok(client) => return Ok(client),
            Err(error) if is_pipe_busy(&error) && started.elapsed() < deadline => {
                tokio::time::sleep(retry_delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(windows)]
fn response_timeout(request: &str) -> Duration {
    let command = if request.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|value| value.get("command")?.as_str().map(str::to_owned))
            .unwrap_or_default()
    } else {
        request
            .split(['|', '\r', '\n'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    match command.as_str() {
        "send_sms" | "SMS" | "sync_sms" => Duration::from_secs(120),
        "check_balance" | "BALANCE" => Duration::from_secs(60),
        "make_call" | "DIAL" | "upload_audio" => Duration::from_secs(180),
        "hang_up" | "HANGUP" => Duration::from_secs(30),
        "update_settings" | "update_integration_settings" => Duration::from_secs(30),
        _ => DEFAULT_RESPONSE_TIMEOUT,
    }
}

#[cfg(windows)]
async fn read_response<R>(reader: &mut R, deadline: Duration) -> Result<String, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut response = String::new();
    let bytes = tokio::time::timeout(deadline, reader.read_line(&mut response))
        .await
        .map_err(|_| {
            format!(
                "The modem service did not respond within {} seconds",
                deadline.as_secs()
            )
        })?
        .map_err(|error| format!("Cannot read response: {error}"))?;
    if bytes == 0 {
        return Err("The modem service closed the connection without a response".into());
    }
    Ok(response)
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[test]
    fn workflow_deadlines_allow_long_modem_operations() {
        assert_eq!(
            response_timeout(r#"{"command":"send_sms"}"#),
            Duration::from_secs(120)
        );
        assert_eq!(
            response_timeout(r#"{"command":"check_balance"}"#),
            Duration::from_secs(60)
        );
        assert_eq!(
            response_timeout(r#"{"command":"make_call"}"#),
            Duration::from_secs(180)
        );
        assert_eq!(
            response_timeout(r#"{"command":"get_settings"}"#),
            DEFAULT_RESPONSE_TIMEOUT
        );
        assert_eq!(
            response_timeout(r#"{"command":"update_settings"}"#),
            Duration::from_secs(30)
        );
        assert_eq!(
            response_timeout(r#"{"command":"update_integration_settings"}"#),
            Duration::from_secs(30)
        );
        assert!(is_pipe_busy(&std::io::Error::from_raw_os_error(231)));
    }

    #[tokio::test]
    async fn stalled_read_times_out_without_blocking_a_later_read() {
        let (_held_writer, stalled_reader) = tokio::io::duplex(32);
        let error = read_response(
            &mut BufReader::new(stalled_reader),
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.contains("did not respond"));

        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"ready\n").await.unwrap();
        let response = read_response(&mut BufReader::new(reader), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(response, "ready\n");
    }

    #[tokio::test]
    async fn transient_pipe_busy_errors_are_retried_within_a_bound() {
        let mut attempts = 0;
        let connected = retry_pipe_busy(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(std::io::Error::from_raw_os_error(231))
                } else {
                    Ok("connected")
                }
            },
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert_eq!(connected, "connected");
        assert_eq!(attempts, 3);

        let started = Instant::now();
        let error = retry_pipe_busy(
            || Err::<(), _>(std::io::Error::from_raw_os_error(231)),
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(is_pipe_busy(&error));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
