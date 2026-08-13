use super::*;

pub(super) async fn configure_delivery_tracking(
    tx: &mpsc::Sender<hardware::AtRequest>,
) -> DeliveryCapability {
    let mut general_errors = Vec::new();
    for (label, command) in [
        ("CMGF", "AT+CMGF=1"),
        ("CPMS", "AT+CPMS=\"SM\",\"SM\",\"SM\""),
    ] {
        if actor_lines(tx, command.into(), None).await.is_err() {
            general_errors.push(format!("{label} configuration failed"));
        }
    }
    let report_request = configure_and_verify(
        tx,
        "CSMP",
        "AT+CSMP=49,167,0,0",
        "AT+CSMP?",
        "+CSMP:49,167,0,0",
    )
    .await;
    let report_request_available = report_request.is_ok();
    let direct_report = configure_and_verify(
        tx,
        "CNMI",
        "AT+CNMI=2,1,0,1,0",
        "AT+CNMI?",
        "+CNMI:2,1,0,1,0",
    )
    .await;
    let direct_report_reception = direct_report.is_ok();
    let stored_report_reception = if direct_report_reception {
        Ok(())
    } else {
        configure_and_verify(
            tx,
            "CNMI",
            "AT+CNMI=2,1,0,2,0",
            "AT+CNMI?",
            "+CNMI:2,1,0,2,0",
        )
        .await
    };
    let report_reception_available = direct_report_reception || stored_report_reception.is_ok();
    let mut errors = general_errors;
    if let Err(error) = report_request {
        errors.push(error);
    }
    if !direct_report_reception && stored_report_reception.is_ok() {
        let reason = direct_report
            .err()
            .unwrap_or_else(|| "CNMI direct delivery reports could not be verified".into());
        errors.push(format!(
            "{reason}; using stored-report synchronization (modem storage must have free slots)"
        ));
    } else if !report_reception_available {
        errors.push(
            stored_report_reception
                .err()
                .unwrap_or_else(|| "CNMI delivery-report reception could not be verified".into()),
        );
    }
    DeliveryCapability {
        attempted: true,
        report_request_available,
        report_reception_available,
        available: report_request_available && report_reception_available,
        error: errors.join("; "),
    }
}

pub(super) async fn configure_and_verify(
    tx: &mpsc::Sender<hardware::AtRequest>,
    _label: &str,
    set_command: &str,
    query_command: &str,
    expected: &str,
) -> Result<(), String> {
    let mut last_error = format!("{_label} configuration could not be verified");
    for attempt in 0..3 {
        match actor_lines(tx, set_command.into(), None).await {
            Ok(_) => {}
            Err(error) => {
                last_error = format!("{_label} configuration failed: {error}");
                continue;
            }
        }
        match actor_lines(tx, query_command.into(), None).await {
            Ok(lines) if lines.iter().any(|line| setting_matches(line, expected)) => {
                return Ok(());
            }
            Ok(lines) => {
                let readback = lines.join(" | ");
                last_error = if readback.is_empty() {
                    format!("{_label} readback was empty")
                } else {
                    format!("{_label} readback did not match: {readback}")
                };
            }
            Err(error) => last_error = format!("{_label} readback failed: {error}"),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Err(last_error)
}

pub(super) fn setting_matches(line: &str, expected: &str) -> bool {
    line.chars()
        .filter(|character| !character.is_ascii_whitespace())
        .eq(expected
            .chars()
            .filter(|character| !character.is_ascii_whitespace()))
}
pub(super) async fn sync_sms_json(
    tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
) -> Result<usize, String> {
    let lines = actor_pdu_snapshot(tx).await?;
    let stamp = now();
    let records = snapshot_records(modemd::sms::parse_cmgl(&lines), stamp);
    store.sync_sms(&records, stamp).map_err(|e| e.to_string())?;
    archive_modem_sms(tx, store, &records, stamp).await?;
    Ok(records.len())
}

pub(super) async fn archive_modem_sms(
    tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
    records: &[SmsRecord],
    now_ms: i64,
) -> Result<(), String> {
    let archived_records = records
        .iter()
        .filter(|record| sms_ready_for_archive(record, now_ms))
        .cloned()
        .collect::<Vec<_>>();
    let commands = archive_commands(&archived_records, now_ms);
    if commands.is_empty() {
        return Ok(());
    }
    let count = commands.len();
    actor_batch_lines(tx, commands, None, Duration::from_secs(90)).await?;
    store
        .mark_sms_archived(&archived_records)
        .map_err(|error| error.to_string())?;
    eprintln!("archived {count} modem SMS storage slots after durable synchronization");
    Ok(())
}

pub(super) const MULTIPART_ARCHIVE_GRACE_MS: i64 = 5 * 60 * 1000;

pub(super) fn sms_ready_for_archive(record: &SmsRecord, now_ms: i64) -> bool {
    record.part_count <= 1
        || record.multipart_complete
        || now_ms.saturating_sub(record.created_at_ms) >= MULTIPART_ARCHIVE_GRACE_MS
}

pub(super) fn archive_commands(records: &[SmsRecord], now_ms: i64) -> Vec<String> {
    let mut indices = records
        .iter()
        .filter(|record| sms_ready_for_archive(record, now_ms))
        .flat_map(|record| record.storage_indices.iter().copied())
        .filter(|index| *index > 0)
        .collect::<Vec<_>>();
    indices.sort_unstable_by(|left, right| right.cmp(left));
    indices.dedup();
    indices
        .into_iter()
        .map(|index| format!("AT+CMGD={index}"))
        .collect()
}

pub(super) fn snapshot_records(parsed: Vec<modemd::sms::SimSms>, stamp: i64) -> Vec<SmsRecord> {
    parsed
        .into_iter()
        .map(|x| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let single_sim_identity = (x.part_count == 1).then_some(x.index);
            let immutable_body = (x.part_count == 1).then_some(x.body.as_str());
            (
                single_sim_identity,
                &x.direction,
                &x.peer,
                immutable_body,
                &x.modem_timestamp,
                &x.kind,
                &x.message_reference,
                &x.delivery_status,
            )
                .hash(&mut h);
            let created_at_ms = modem_timestamp_ms(&x.modem_timestamp).unwrap_or(stamp);
            SmsRecord {
                id: ulid::Ulid::new().to_string(),
                direction: x.direction,
                peer: x.peer,
                body: x.body,
                state: match x.modem_status.as_str() {
                    "REC UNREAD" => "unread",
                    "REC READ" => "read",
                    "STO SENT" => "submitted",
                    "STO UNSENT" => "send-failed",
                    _ => "status-report",
                }
                .into(),
                message_reference: x.message_reference,
                created_at_ms,
                kind: x.kind,
                source: "sim".into(),
                storage: "SM".into(),
                storage_index: x.index,
                storage_indices: x.storage_indices,
                part_count: x.part_count,
                parts_received: x.parts_received,
                multipart_complete: x.multipart_complete,
                part_payloads: x.part_payloads,
                part_timestamps: x.part_timestamps,
                modem_status: x.modem_status,
                modem_timestamp: x.modem_timestamp.clone(),
                encoding: x.encoding,
                dcs: x.dcs,
                length: x.length,
                service_center: x.service_center,
                delivery_status: x.delivery_status,
                delivery_report_scts: x.modem_timestamp.clone(),
                delivery_report_discharge_time: x.discharge_time,
                synchronized_at_ms: stamp,
                present_on_modem: true,
                fingerprint: format!("{:016x}", h.finish()),
                ..Default::default()
            }
        })
        .collect()
}

pub(super) fn modem_timestamp_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[2] != b'/'
        || bytes[5] != b'/'
        || bytes[8] != b','
        || bytes[11] != b':'
        || bytes[14] != b':'
        || !matches!(bytes[17], b'+' | b'-')
    {
        return None;
    }
    let number = |start: usize| -> Option<i64> {
        std::str::from_utf8(&bytes[start..start + 2])
            .ok()?
            .parse()
            .ok()
    };
    let (year, month, day) = (2000 + number(0)?, number(3)?, number(6)?);
    let (hour, minute, second) = (number(9)?, number(12)?, number(15)?);
    let quarters = number(18)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    if !(1..=12).contains(&month)
        || !(1..=max_day).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
        || quarters > 79
    {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100) + doy - 719468;
    let local_seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    let offset = quarters * 15 * 60 * if bytes[17] == b'-' { -1 } else { 1 };
    Some((local_seconds - offset) * 1000)
}
pub(super) async fn actor_pdu_snapshot(
    tx: &mpsc::Sender<hardware::AtRequest>,
) -> Result<Vec<String>, String> {
    actor_batch_lines(
        tx,
        vec![
            "AT+CPMS=\"SM\",\"SM\",\"SM\"".into(),
            "AT+CMGF=0".into(),
            "AT+CMGL=4".into(),
        ],
        Some("AT+CMGF=1".into()),
        Duration::from_secs(35),
    )
    .await
}

pub(super) async fn actor_batch_lines(
    tx: &mpsc::Sender<hardware::AtRequest>,
    batch: Vec<String>,
    finalizer: Option<String>,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(hardware::AtRequest {
        command: String::new(),
        payload: None,
        guarded: false,
        payload_mode: PayloadMode::Sms,
        batch,
        finalizer,
        reply,
    })
    .map_err(|_| "modem command actor unavailable".to_owned())?;
    tokio::time::timeout(timeout, rx)
        .await
        .map_err(|_| "modem command timed out".to_owned())?
        .map_err(|_| "modem command actor stopped".to_owned())?
        .map_err(|e| e.to_string())?
        .into_lines()
        .map_err(|e| e.to_string())
}
