use super::*;

pub(super) fn reconcile_legacy_parts(
    tx: &rusqlite::Transaction<'_>,
    logical: &SmsRecord,
    logical_id: &str,
) -> Result<(), ModemError> {
    if logical.storage_indices.len() <= 1 {
        return Ok(());
    }
    for (part, index) in logical.storage_indices.iter().enumerate() {
        let mut statement=tx.prepare("SELECT id,modem_timestamp,body,encoding,dcs FROM sms WHERE source='sim' AND id<>?1 AND storage=?2 AND storage_index=?3 AND peer=?4 AND superseded=0").map_err(db_error)?;
        let candidates = statement
            .query_map(
                params![logical_id, logical.storage, index, logical.peer],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i32>(4)?,
                    ))
                },
            )
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let expected_payload = logical
            .part_payloads
            .get(part)
            .map(String::as_str)
            .unwrap_or_default();
        let expected_timestamp = logical
            .part_timestamps
            .get(part)
            .map(String::as_str)
            .unwrap_or(&logical.modem_timestamp);
        for (id, timestamp, body, encoding, dcs) in candidates {
            let explicit =
                encoding.eq_ignore_ascii_case("UCS2") || (dcs >= 0 && sms_dcs_uses_ucs2(dcs as u8));
            let normalized = crate::sms::decode_ucs2_body(&body, explicit).unwrap_or(body);
            if normalize_modem_timestamp(&timestamp)
                == normalize_modem_timestamp(expected_timestamp)
                && normalized == expected_payload
            {
                tx.execute("UPDATE sms SET superseded=1 WHERE id=?1", [id])
                    .map_err(db_error)?;
            }
        }
    }
    Ok(())
}

pub(super) fn normalize_modem_timestamp(value: &str) -> &str {
    value.get(..17).unwrap_or(value)
}

pub(super) fn apply_delivery_reports(
    tx: &rusqlite::Transaction<'_>,
    records: &[SmsRecord],
    synchronized_at_ms: i64,
) -> Result<usize, ModemError> {
    let mut matched_count = 0;
    for report in records.iter().filter(|record| {
        record.kind == "status-report"
            && !record.peer.is_empty()
            && !record.message_reference.is_empty()
    }) {
        let state = delivery_state(&report.delivery_status);
        let scts_ms = modem_timestamp_ms(&report.delivery_report_scts);
        let report_sync = if report.synchronized_at_ms == 0 {
            synchronized_at_ms
        } else {
            report.synchronized_at_ms
        };
        let event_ms =
            modem_timestamp_ms(&report.delivery_report_discharge_time).unwrap_or(report_sync);

        // Once a report has been linked, an idempotent replay with the same
        // SCTS follows that link even if TP-MR has since wrapped around.
        let existing_link: Option<String> = tx
            .query_row(
                "SELECT matched_sms_id FROM sms WHERE kind='status-report' AND message_reference=?1 AND peer=?2 AND delivery_report_scts=?3 AND matched_sms_id<>'' LIMIT 1",
                params![report.message_reference, report.peer, report.delivery_report_scts],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let mut statement = tx.prepare(
            "SELECT id,peer,created_at_ms FROM sms WHERE source='app' AND direction='outbound' AND message_reference=?1 AND kind<>'status-report'",
        ).map_err(db_error)?;
        let candidates = statement
            .query_map([&report.message_reference], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let eligible = candidates
            .into_iter()
            .filter(|(_, peer, created)| {
                if normalize_peer(peer) != normalize_peer(&report.peer) {
                    return false;
                }
                scts_ms.map_or_else(
                    || *created <= report_sync,
                    |scts| (*created - scts).abs() <= 600_000,
                )
            })
            .collect::<Vec<_>>();
        let matched =
            existing_link.or_else(|| (eligible.len() == 1).then(|| eligible[0].0.clone()));
        if let Some(id) = matched {
            tx.execute(
                "UPDATE sms SET state=?1,delivery_status=?2,delivery_report_scts=?3,delivery_report_discharge_time=?4,delivery_event_ms=?5 WHERE id=?6 AND (delivery_event_ms<?5 OR (delivery_event_ms=?5 AND state IN ('submitted','delivery-pending','delivery-unknown') AND ?1 IN ('delivered','delivery-failed')))",
                params![state, report.delivery_status, report.delivery_report_scts, report.delivery_report_discharge_time, event_ms, id],
            )
            .map_err(db_error)?;
            tx.execute(
                "UPDATE sms SET matched_sms_id=?1 WHERE id=?2",
                params![id, report.id],
            )
            .map_err(db_error)?;
            matched_count += 1;
        }
    }
    Ok(matched_count)
}

pub(super) fn reconcile_stored_submissions(
    tx: &rusqlite::Transaction<'_>,
    records: &[SmsRecord],
) -> Result<(), ModemError> {
    for stored in records.iter().filter(|record| {
        record.direction == "outbound"
            && record.modem_status == "STO SENT"
            && !record.body.is_empty()
    }) {
        let normalized_peer = normalize_peer(&stored.peer);
        let mut statement = tx.prepare(
            "SELECT id,peer FROM sms WHERE source='app' AND direction='outbound' AND body=?1 AND (?2='' OR message_reference=?2) ORDER BY created_at_ms DESC,id DESC",
        ).map_err(db_error)?;
        let candidates = statement
            .query_map(params![stored.body, stored.message_reference], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        if let Some((app_id, _)) = candidates
            .into_iter()
            .find(|(_, peer)| normalize_peer(peer) == normalized_peer)
        {
            tx.execute(
                "UPDATE sms SET superseded=1 WHERE source='sim' AND storage=?1 AND storage_index=?2 AND fingerprint=?3",
                params![stored.storage, stored.storage_index, stored.fingerprint],
            ).map_err(db_error)?;
            tx.execute(
                "UPDATE sms SET modem_status=?1,synchronized_at_ms=?2,present_on_modem=1 WHERE id=?3",
                params![stored.modem_status, stored.synchronized_at_ms, app_id],
            ).map_err(db_error)?;
        }
    }
    Ok(())
}

pub(super) fn normalize_peer(value: &str) -> String {
    if let Ok(peer) = crate::sms::normalize_sms_destination(value) {
        return peer;
    }
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    digits.strip_prefix("00").unwrap_or(&digits).to_owned()
}

pub(super) fn delivery_state(status: &str) -> &'static str {
    let code = status
        .strip_prefix("0x")
        .or_else(|| status.strip_prefix("0X"))
        .and_then(|value| u8::from_str_radix(value, 16).ok());
    match code {
        Some(0x00) => "delivered",
        Some(0x01..=0x1f) => "delivery-unknown",
        Some(0x20..=0x3f) => "delivery-pending",
        Some(0x40..=0x7f) => "delivery-failed",
        Some(0x80..=0xff) => "delivery-unknown",
        None => "delivery-unknown",
    }
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
    let number = |start: usize| {
        std::str::from_utf8(&bytes[start..start + 2])
            .ok()?
            .parse::<i64>()
            .ok()
    };
    let (year, month, day) = (2000 + number(0)?, number(3)?, number(6)?);
    let (hour, minute, second, quarters) = (number(9)?, number(12)?, number(15)?, number(18)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
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
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    let offset = quarters * 900 * if bytes[17] == b'-' { -1 } else { 1 };
    Some((seconds - offset) * 1000)
}
pub(super) fn sms_dcs_uses_ucs2(dcs: u8) -> bool {
    (dcs & 0xc0 == 0 && dcs & 0x0c == 8) || dcs & 0xf0 == 0xe0
}
