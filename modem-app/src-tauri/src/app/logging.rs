use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn log_event(area: &str, message: impl AsRef<str>) {
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

pub(super) fn logged_request(request: &str) -> String {
    if request.starts_with('{') {
        let command = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|v| v.get("command").and_then(|x| x.as_str()).map(str::to_owned))
            .unwrap_or_else(|| "invalid".into());
        format!("JSON {command} (SMS and balance content redacted)")
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

pub(super) fn logged_response(request: &str, response: &str) -> String {
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

pub(super) fn suppress_successful_poll_log(request: &str) -> bool {
    if request == "STATUS" {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(|command| command.as_str())
                .map(|command| {
                    matches!(
                        command,
                        "list_calls" | "get_current_audio" | "list_audio" | "get_call_data"
                    )
                })
        })
        .unwrap_or(false)
}
