use crate::ModemError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimSms {
    pub index: i32,
    pub modem_status: String,
    pub direction: String,
    pub kind: String,
    pub peer: String,
    pub body: String,
    pub modem_timestamp: String,
    pub encoding: String,
    pub dcs: i32,
    pub length: i32,
    pub service_center: String,
    pub message_reference: String,
    pub delivery_status: String,
}

pub fn parse_cmgl(lines: &[String]) -> Vec<SimSms> {
    let mut records = Vec::new();
    let mut current: Option<SimSms> = None;
    for line in lines {
        if let Some(header) = line.strip_prefix("+CMGL:") {
            if let Some(record) = current.take() {
                records.push(decode_record(record));
            }
            current = parse_header(header.trim());
        } else if !matches!(line.trim(), "OK" | "ERROR") {
            if let Some(record) = current.as_mut() {
                if !record.body.is_empty() {
                    record.body.push('\n');
                }
                record.body.push_str(line);
            }
        }
    }
    if let Some(record) = current {
        records.push(decode_record(record));
    }
    records
}

fn parse_header(header: &str) -> Option<SimSms> {
    let fields = quoted_csv(header);
    let index = fields.first()?.parse().ok()?;
    let status = fields.get(1).cloned().unwrap_or_default();
    let (direction, kind) = match status.as_str() {
        "REC UNREAD" | "REC READ" => ("inbound", "received"),
        "STO SENT" | "STO UNSENT" => ("outbound", "stored"),
        value if value.contains("REPORT") => ("inbound", "status-report"),
        _ => ("unknown", "unknown"),
    };
    let peer = fields.get(2).cloned().unwrap_or_default();
    let modem_timestamp = fields
        .iter()
        .skip(3)
        .find(|v| v.contains('/') && v.contains(':'))
        .cloned()
        .unwrap_or_default();
    // In the AT+CSDH=1 form, DCS is field 8. Numeric fields after it are
    // address metadata and message length, so searching from the end is wrong.
    let dcs = fields
        .get(8)
        .and_then(|value| parse_dcs(value))
        .unwrap_or(-1);
    Some(SimSms {
        index,
        modem_status: status,
        direction: direction.into(),
        kind: kind.into(),
        peer,
        modem_timestamp,
        dcs,
        encoding: if dcs >= 0 && dcs_uses_ucs2(dcs as u8) {
            "UCS2"
        } else {
            "GSM"
        }
        .into(),
        ..Default::default()
    })
}

fn quoted_csv(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut value = String::new();
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                result.push(value.trim().to_owned());
                value.clear();
            }
            _ => value.push(c),
        }
    }
    result.push(value.trim().to_owned());
    result
}

fn decode_record(mut record: SimSms) -> SimSms {
    let explicit_ucs2 = record.encoding == "UCS2";
    if let Some(decoded) = decode_ucs2_body(&record.body, explicit_ucs2) {
        record.body = decoded;
        record.encoding = "UCS2".into();
    }
    record.length = record.body.chars().count() as i32;
    record
}

/// Decode modem UCS2 hex. Without DCS metadata, only plausible UTF-16 text is
/// decoded so ordinary GSM content that happens to contain hex digits is safe.
pub fn decode_ucs2_body(body: &str, explicit_ucs2: bool) -> Option<String> {
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty()
        || compact.len() % 4 != 0
        || !compact.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return explicit_ucs2.then(|| format!("[hex] {compact}"));
    }
    let units = (0..compact.len())
        .step_by(4)
        .map(|index| u16::from_str_radix(&compact[index..index + 4], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let decoded = String::from_utf16(&units).ok()?;
    let decoded = decoded
        .strip_prefix('\u{feff}')
        .unwrap_or(&decoded)
        .to_owned();
    let plausible = decoded.chars().any(|c| c.is_alphabetic())
        && decoded
            .chars()
            .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'));
    (explicit_ucs2 || plausible).then_some(decoded)
}

fn parse_dcs(value: &str) -> Option<i32> {
    let value = value.trim().trim_matches('"');
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse().ok(),
            |hex| i32::from_str_radix(hex, 16).ok(),
        )
}

fn dcs_uses_ucs2(dcs: u8) -> bool {
    (dcs & 0xc0 == 0 && dcs & 0x0c == 0x08) || dcs & 0xf0 == 0xe0
}

pub fn normalize_number(input: &str) -> Result<String, ModemError> {
    let compact: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')'))
        .collect();
    let digits = compact.strip_prefix('+').unwrap_or(&compact);
    if !compact.starts_with('+')
        || !(7..=15).contains(&digits.len())
        || !digits.chars().all(|c| c.is_ascii_digit())
    {
        return Err(ModemError::Validation(
            "use an international number such as +66812345678".into(),
        ));
    }
    Ok(compact)
}

pub fn validate_body(body: &str) -> Result<&'static str, ModemError> {
    if body.is_empty() {
        return Err(ModemError::Validation("message cannot be empty".into()));
    }
    let gsm7 = body.chars().all(|c| c.is_ascii() && !c.is_ascii_control());
    let units = body.encode_utf16().count();
    if (gsm7 && body.chars().count() <= 160) || (!gsm7 && units <= 70) {
        Ok(if gsm7 { "GSM-7" } else { "UCS2" })
    } else {
        Err(ModemError::Validation(
            "multipart SMS not supported in v1".into(),
        ))
    }
}

pub fn ucs2_hex(body: &str) -> String {
    body.encode_utf16().map(|u| format!("{u:04X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_numbers_and_limits() {
        assert_eq!(normalize_number("+66 81-234-5678").unwrap(), "+66812345678");
        assert!(normalize_number("08123").is_err());
        assert_eq!(validate_body("hello").unwrap(), "GSM-7");
        assert_eq!(validate_body("สวัสดี").unwrap(), "UCS2");
        assert!(validate_body(&"x".repeat(161)).is_err());
    }
    #[test]
    fn encodes_ucs2() {
        assert_eq!(ucs2_hex("Aก"), "00410E01");
    }
    #[test]
    fn parses_all_statuses_multiline_and_ucs2() {
        let lines = vec![
            "+CMGL: 1,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\"".into(),
            "line one".into(),
            "line two".into(),
            "+CMGL: 2,\"STO SENT\",\"+66123456789\",\"\",\"26/08/03,10:00:00+28\",129,0,0,8,\"+84000000000\",145,4".into(),
            "0056006900650074".into(),
            "+CMGL: 3,\"STO UNSENT\",\"+66111111111\"".into(),
            "draft".into(),
            "+CMGL: 4,\"STATUS REPORT\",\"+66222222222\"".into(),
            "delivered".into(),
            "OK".into(),
        ];
        let parsed = parse_cmgl(&lines);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].body, "line one\nline two");
        assert_eq!(parsed[1].body, "Viet");
        assert_eq!(parsed[2].modem_status, "STO UNSENT");
        assert_eq!(parsed[3].kind, "status-report");
    }
    #[test]
    fn malformed_ucs2_is_lossless_hex() {
        let x = parse_cmgl(&[
            "+CMGL: 1,\"REC READ\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,8".into(),
            "0041ZZ".into(),
        ]);
        assert_eq!(x[0].body, "[hex] 0041ZZ");
    }
    #[test]
    fn extended_header_uses_dcs_instead_of_trailing_length() {
        let x = parse_cmgl(&[
            "+CMGL: 9,\"REC READ\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,\"0x08\",\"+84000000000\",145,67".into(),
            "00540068007500EA002000620061006F".into(),
            "OK".into(),
        ]);
        assert_eq!(x[0].body, "Thuê bao");
        assert_eq!(x[0].dcs, 8);
        assert_eq!(x[0].encoding, "UCS2");
    }
    #[test]
    fn infers_ucs2_when_modem_omits_extended_header() {
        let x = parse_cmgl(&[
            "+CMGL: 11,\"REC READ\",\"191\",\"\",\"26/08/03,10:00:00+28\"".into(),
            "FEFF00540068007500EA002000620061006F".into(),
        ]);
        assert_eq!(x[0].body, "Thuê bao");
    }
}
