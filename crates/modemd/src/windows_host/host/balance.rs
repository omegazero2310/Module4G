use super::*;

pub(super) async fn balance_json(
    tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
) -> Result<BalanceRecord, String> {
    let (raw, sms_id) = check_viettel_balance(tx, store).await?;
    let stamp = now();
    let b = BalanceRecord {
        id: ulid::Ulid::new().to_string(),
        raw,
        created_at_ms: stamp,
        sms_id,
        ..Default::default()
    };
    store.save_balance(&b).map_err(|e| e.to_string())?;
    Ok(b)
}

pub(super) async fn run_actor(
    command_tx: &mpsc::Sender<hardware::AtRequest>,
    command: String,
    payload: Option<Vec<u8>>,
    guarded: bool,
) -> String {
    let (reply, response) = tokio::sync::oneshot::channel();
    if command_tx
        .send(hardware::AtRequest {
            command,
            payload,
            guarded,
            payload_mode: PayloadMode::Sms,
            batch: Vec::new(),
            finalizer: None,
            reply,
        })
        .is_err()
    {
        return "ERROR: modem command actor unavailable\n".into();
    }
    match tokio::time::timeout(Duration::from_secs(35), response).await {
        Ok(Ok(Ok(response))) => match response.into_lines() {
            Ok(lines) => format!("{}\n", lines.join(" | ")),
            Err(error) => format!("ERROR: {error}\n"),
        },
        Ok(Ok(Err(error))) => format!("ERROR: {error}\n"),
        Ok(Err(_)) => "ERROR: modem command actor stopped\n".into(),
        Err(_) => "ERROR: command timed out\n".into(),
    }
}

pub(super) async fn dial_with_release_retry(
    command_tx: &mpsc::Sender<hardware::AtRequest>,
    number: &str,
) -> String {
    const MAX_ATTEMPTS: usize = 5;
    const RELEASE_RETRY_DELAY: Duration = Duration::from_millis(500);

    let command = format!("ATD{number};");
    for attempt in 1..=MAX_ATTEMPTS {
        let response = run_actor(command_tx, command.clone(), None, false).await;
        if !is_transient_call_release_error(&response) || attempt == MAX_ATTEMPTS {
            return response;
        }
        tokio::time::sleep(RELEASE_RETRY_DELAY).await;
    }
    unreachable!("the bounded dial loop always returns")
}

pub(super) fn is_transient_call_release_error(response: &str) -> bool {
    let response = response.to_ascii_lowercase();
    response.contains("+cme error:") && response.contains("operation not allowed")
}

pub(super) async fn check_viettel_balance(
    command_tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
) -> Result<(String, String), String> {
    use std::collections::HashSet;
    let baseline = actor_pdu_snapshot(command_tx).await?;
    let baseline_records = snapshot_records(modemd::sms::parse_cmgl(&baseline), now());
    let persisted_baseline = store
        .list_sms(usize::MAX)
        .map_err(|error| error.to_string())?;
    let identities: HashSet<String> = persisted_baseline
        .iter()
        .chain(&baseline_records)
        .map(|record| record.fingerprint.clone())
        .collect();

    let submitted = actor_lines(command_tx, "AT+CMGS=\"191\"".into(), Some(b"TK".to_vec())).await?;
    if !submitted.iter().any(|line| line.starts_with("+CMGS:")) {
        return Err("modem returned OK without accepting the TK submission".into());
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        match actor_pdu_snapshot(command_tx).await {
            Ok(lines) => {
                let stamp = now();
                let records = snapshot_records(modemd::sms::parse_cmgl(&lines), stamp);
                if find_balance_candidate(&records, &identities).is_some() {
                    store
                        .sync_sms(&records, stamp)
                        .map_err(|error| error.to_string())?;
                }
            }
            Err(error) => return Err(error),
        }
        if let Some(message) = find_persisted_balance_candidate(store, &identities)? {
            return Ok((message.body, message.id));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if let Some(message) = find_persisted_balance_candidate(store, &identities)? {
        return Ok((message.body, message.id));
    }
    Err("timed out waiting for a new complete Viettel balance SMS from 191".into())
}

pub(super) fn find_persisted_balance_candidate(
    store: &Store,
    baseline: &std::collections::HashSet<String>,
) -> Result<Option<SmsRecord>, String> {
    let records = store
        .list_sms(usize::MAX)
        .map_err(|error| error.to_string())?;
    Ok(find_balance_candidate(&records, baseline).cloned())
}

pub(super) fn find_balance_candidate<'a>(
    records: &'a [SmsRecord],
    baseline: &std::collections::HashSet<String>,
) -> Option<&'a SmsRecord> {
    records.iter().find(|record| {
        !baseline.contains(&record.fingerprint)
            && record.direction == "inbound"
            && record.peer == "191"
            && record.multipart_complete
            && is_viettel_balance_body(&record.body)
    })
}

pub(super) fn is_viettel_balance_body(body: &str) -> bool {
    let folded = body
        .to_lowercase()
        .replace(
            [
                'á', 'à', 'ả', 'ã', 'ạ', 'ă', 'ắ', 'ằ', 'ẳ', 'ẵ', 'ặ', 'â', 'ấ', 'ầ', 'ẩ', 'ẫ', 'ậ',
            ],
            "a",
        )
        .replace(
            ['ố', 'ồ', 'ổ', 'ỗ', 'ộ', 'ô', 'ớ', 'ờ', 'ở', 'ỡ', 'ợ', 'ơ'],
            "o",
        )
        .replace(['ố'], "o")
        .replace(['ư', 'ứ', 'ừ', 'ử', 'ữ', 'ự'], "u")
        .replace('đ', "d");
    folded.contains("tk goc") || folded.contains("tai khoan") || folded.contains("so du")
}

#[cfg(test)]
pub(super) fn viettel_balance_message(response: &str) -> Option<String> {
    let parts: Vec<_> = response.split(" | ").map(str::trim).collect();
    let header = parts.iter().position(|part| {
        part.starts_with("+CMGL:")
            && csv_field(part, 2)
                .is_some_and(|sender| sender.trim().trim_matches('\"').eq_ignore_ascii_case("191"))
    })?;
    let body: Vec<_> = parts[header + 1..]
        .iter()
        .take_while(|part| !part.starts_with("+CMGL:") && **part != "OK")
        .copied()
        .collect();
    if body.is_empty() {
        return None;
    }

    let body = body.join("\n");
    let dcs = csv_field(parts[header], 8).and_then(parse_dcs);
    let compact_hex: String = body
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let decoded = decode_ucs2_hex(&compact_hex);
    let is_ucs2 = dcs.is_some_and(dcs_uses_ucs2)
        || (dcs.is_none() && decoded.as_deref().is_some_and(looks_like_text));
    Some(if is_ucs2 {
        // A malformed or truncated UCS2 response should still be visible to the
        // caller instead of making the already-read SMS disappear.
        decoded.unwrap_or(body)
    } else {
        body
    })
}

#[cfg(test)]
pub(super) fn parse_dcs(value: &str) -> Option<u8> {
    let value = value.trim().trim_matches('"');
    value.parse().ok().or_else(|| {
        value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
    })
}

#[cfg(test)]
pub(super) fn looks_like_text(value: &str) -> bool {
    let value = value.strip_prefix('\u{feff}').unwrap_or(value);
    !value.is_empty()
        && value.chars().any(|character| character.is_alphabetic())
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
}

#[cfg(test)]
pub(super) fn dcs_uses_ucs2(dcs: u8) -> bool {
    (dcs & 0xc0 == 0 && dcs & 0x0c == 0x08) || dcs & 0xf0 == 0xe0
}

#[cfg(test)]
pub(super) fn csv_field(value: &str, wanted: usize) -> Option<&str> {
    let mut quoted = false;
    let mut field = 0;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                if field == wanted {
                    return Some(value[start..index].trim());
                }
                field += 1;
                start = index + 1;
            }
            _ => {}
        }
    }
    (field == wanted).then(|| value[start..].trim())
}

#[cfg(test)]
pub(super) fn decode_ucs2_hex(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() % 4 != 0
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let units = (0..value.len())
        .step_by(4)
        .map(|index| u16::from_str_radix(&value[index..index + 4], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
        .map(|decoded| {
            decoded
                .strip_prefix('\u{feff}')
                .unwrap_or(&decoded)
                .to_owned()
        })
}

pub(super) fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}
