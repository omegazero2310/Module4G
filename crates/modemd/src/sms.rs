use crate::ModemError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimSms {
    pub index: i32,
    pub storage_indices: Vec<i32>,
    pub part_count: i32,
    pub parts_received: i32,
    pub multipart_complete: bool,
    #[serde(skip)]
    pub part_payloads: Vec<String>,
    #[serde(skip)]
    pub part_timestamps: Vec<String>,
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
    #[serde(skip)]
    concat: Option<Concat>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Concat {
    sixteen_bit: bool,
    reference: u16,
    total: u8,
    sequence: u8,
}

/// Parse a `+CMGL` PDU-mode snapshot and assemble only segments explicitly
/// related by a 3GPP concatenation UDH.
pub fn parse_cmgl(lines: &[String]) -> Vec<SimSms> {
    let mut physical = Vec::new();
    let mut header: Option<(i32, String)> = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("+CMGL:") {
            let fields = quoted_csv(value.trim());
            header = fields.first().and_then(|x| x.parse().ok()).map(|index| {
                (
                    index,
                    normalize_modem_status(fields.get(1).map(String::as_str).unwrap_or_default()),
                )
            });
        } else if !matches!(line.trim(), "" | "OK" | "ERROR") {
            if let Some((index, status)) = header.take() {
                physical.push(decode_pdu(index, status, line.trim()));
            }
        }
    }
    assemble(physical)
}

fn normalize_modem_status(value: &str) -> String {
    match value.trim().trim_matches('"') {
        "0" => "REC UNREAD",
        "1" => "REC READ",
        "2" => "STO UNSENT",
        "3" => "STO SENT",
        other => other,
    }
    .to_owned()
}

fn decode_pdu(index: i32, status: String, source: &str) -> SimSms {
    let fallback = || SimSms {
        index,
        storage_indices: vec![index],
        part_count: 1,
        parts_received: 1,
        multipart_complete: true,
        part_payloads: vec![format!("[hex] {}", source.trim())],
        part_timestamps: Vec::new(),
        modem_status: status.clone(),
        direction: "unknown".into(),
        kind: "unsupported-pdu".into(),
        body: format!("[hex] {}", source.trim()),
        encoding: "binary/unknown".into(),
        length: source.trim().len() as i32 / 2,
        ..Default::default()
    };
    let Some(bytes) = decode_hex(source) else {
        return fallback();
    };
    let mut p = Cursor::new(&bytes);
    let Some(smsc_len) = p.byte() else {
        return fallback();
    };
    let service_center = if smsc_len == 0 {
        String::new()
    } else {
        let Some(raw) = p.take(smsc_len as usize) else {
            return fallback();
        };
        if raw.is_empty() {
            String::new()
        } else {
            decode_address(&raw[1..], (raw.len() - 1) * 2, raw[0])
        }
    };
    let Some(first) = p.byte() else {
        return fallback();
    };
    let udhi = first & 0x40 != 0;
    let mti = first & 3;
    let parsed = match mti {
        0 => parse_deliver(&mut p, first, udhi),
        1 => parse_submit(&mut p, first, udhi),
        2 => parse_status_report(&mut p),
        _ => None,
    };
    let Some(mut record) = parsed else {
        return fallback();
    };
    record.index = index;
    record.storage_indices = vec![index];
    record.part_count = record.concat.as_ref().map_or(1, |x| x.total as i32);
    record.parts_received = 1;
    record.multipart_complete = record.part_count == 1;
    record.part_payloads = vec![record.body.clone()];
    record.part_timestamps = vec![record.modem_timestamp.clone()];
    record.modem_status = status;
    record.service_center = service_center;
    record.length = record.body.chars().count() as i32;
    record
}

fn parse_deliver(p: &mut Cursor<'_>, _first: u8, udhi: bool) -> Option<SimSms> {
    let peer = p.address()?;
    let _pid = p.byte()?;
    let dcs = p.byte()?;
    let timestamp = decode_timestamp(p.take(7)?);
    let udl = p.byte()?;
    let (body, encoding, concat) = decode_user_data(p.remaining(), udl, dcs, udhi);
    Some(SimSms {
        direction: "inbound".into(),
        kind: "received".into(),
        peer,
        body,
        modem_timestamp: timestamp,
        encoding,
        dcs: dcs.into(),
        concat,
        ..Default::default()
    })
}

fn parse_submit(p: &mut Cursor<'_>, first: u8, udhi: bool) -> Option<SimSms> {
    let mr = p.byte()?;
    let peer = p.address()?;
    let _pid = p.byte()?;
    let dcs = p.byte()?;
    match (first >> 3) & 3 {
        0 => {}
        2 => {
            p.take(1)?;
        }
        1 | 3 => {
            p.take(7)?;
        }
        _ => unreachable!(),
    }
    let udl = p.byte()?;
    let (body, encoding, concat) = decode_user_data(p.remaining(), udl, dcs, udhi);
    Some(SimSms {
        direction: "outbound".into(),
        kind: "stored".into(),
        peer,
        body,
        encoding,
        dcs: dcs.into(),
        message_reference: mr.to_string(),
        concat,
        ..Default::default()
    })
}

fn parse_status_report(p: &mut Cursor<'_>) -> Option<SimSms> {
    let mr = p.byte()?;
    let peer = p.address()?;
    let timestamp = decode_timestamp(p.take(7)?);
    let _discharge = p.take(7)?;
    let status = p.byte()?;
    Some(SimSms {
        direction: "inbound".into(),
        kind: "status-report".into(),
        peer,
        modem_timestamp: timestamp,
        message_reference: mr.to_string(),
        delivery_status: format!("0x{status:02X}"),
        encoding: "status".into(),
        ..Default::default()
    })
}

fn decode_user_data(data: &[u8], udl: u8, dcs: u8, udhi: bool) -> (String, String, Option<Concat>) {
    let header_len = if udhi {
        data.first().map_or(0, |x| *x as usize + 1)
    } else {
        0
    };
    if header_len > data.len() {
        return (
            format!("[hex] {}", encode_hex(data)),
            "malformed".into(),
            None,
        );
    }
    let concat = if udhi {
        parse_udh(&data[..header_len])
    } else {
        None
    };
    match alphabet(dcs) {
        Alphabet::Gsm7 => {
            let header_septets = (header_len * 8).div_ceil(7);
            let count = (udl as usize).saturating_sub(header_septets);
            (
                decode_gsm7(data, header_septets * 7, count),
                "GSM-7".into(),
                concat,
            )
        }
        Alphabet::Ucs2 => {
            let payload = &data[header_len
                ..data
                    .len()
                    .min(header_len + udl as usize - header_len.min(udl as usize))];
            let body = if payload.len() % 2 == 0 {
                String::from_utf16(
                    &payload
                        .chunks_exact(2)
                        .map(|x| u16::from_be_bytes([x[0], x[1]]))
                        .collect::<Vec<_>>(),
                )
                .ok()
            } else {
                None
            };
            (
                body.unwrap_or_else(|| format!("[hex] {}", encode_hex(payload))),
                "UCS2".into(),
                concat,
            )
        }
        Alphabet::EightBit => (
            format!("[hex] {}", encode_hex(&data[header_len..])),
            "8-bit".into(),
            concat,
        ),
    }
}

enum Alphabet {
    Gsm7,
    EightBit,
    Ucs2,
}
fn alphabet(dcs: u8) -> Alphabet {
    if dcs & 0xc0 == 0 {
        match dcs & 0x0c {
            4 => Alphabet::EightBit,
            8 => Alphabet::Ucs2,
            _ => Alphabet::Gsm7,
        }
    } else if dcs & 0xf0 == 0xe0 {
        Alphabet::Ucs2
    } else {
        Alphabet::Gsm7
    }
}

fn parse_udh(data: &[u8]) -> Option<Concat> {
    let end = data.first().map_or(0, |x| *x as usize + 1).min(data.len());
    let mut i = 1;
    while i + 2 <= end {
        let iei = data[i];
        let len = data[i + 1] as usize;
        i += 2;
        if i + len > end {
            return None;
        }
        let result = match (iei, len) {
            (0, 3) => Some(Concat {
                sixteen_bit: false,
                reference: data[i] as u16,
                total: data[i + 1],
                sequence: data[i + 2],
            }),
            (8, 4) => Some(Concat {
                sixteen_bit: true,
                reference: u16::from_be_bytes([data[i], data[i + 1]]),
                total: data[i + 2],
                sequence: data[i + 3],
            }),
            _ => None,
        };
        if result.is_some() {
            return result;
        }
        i += len;
    }
    None
}

fn assemble(records: Vec<SimSms>) -> Vec<SimSms> {
    type Key = (String, String, bool, u16, u8, i32);
    let mut singles = Vec::new();
    let mut groups: HashMap<Key, BTreeMap<u8, SimSms>> = HashMap::new();
    for r in records {
        if let Some(c) = r.concat.clone() {
            let key = (
                r.direction.clone(),
                r.peer.clone(),
                c.sixteen_bit,
                c.reference,
                c.total,
                r.dcs,
            );
            groups
                .entry(key)
                .or_default()
                .entry(c.sequence)
                .or_insert(r);
        } else {
            singles.push(r);
        }
    }
    for ((_, _, _, reference, total, _), parts) in groups {
        let mut values = parts.into_values();
        let Some(mut r) = values.next() else { continue };
        let mut all = vec![r.clone()];
        all.extend(values);
        all.sort_by_key(|x| x.concat.as_ref().map(|c| c.sequence).unwrap_or(0));
        r.index = all[0].index;
        r.storage_indices = all.iter().map(|x| x.index).collect();
        r.part_payloads = all.iter().map(|x| x.body.clone()).collect();
        r.part_timestamps = all.iter().map(|x| x.modem_timestamp.clone()).collect();
        r.body = r.part_payloads.concat();
        r.part_count = total as i32;
        r.parts_received = all.len() as i32;
        r.multipart_complete = r.parts_received == r.part_count;
        r.message_reference = reference.to_string();
        r.length = r.body.chars().count() as i32;
        singles.push(r);
    }
    singles.sort_by_key(|x| x.index);
    singles
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn byte(&mut self) -> Option<u8> {
        let x = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(x)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let x = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(x)
    }
    fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
    fn address(&mut self) -> Option<String> {
        let digits = self.byte()? as usize;
        let toa = self.byte()?;
        let raw = self.take(digits.div_ceil(2))?;
        Some(decode_address(raw, digits, toa))
    }
}
fn decode_address(raw: &[u8], digits: usize, toa: u8) -> String {
    if toa & 0x70 == 0x50 {
        return decode_gsm7(raw, 0, digits * 4 / 7);
    }
    let mut s = String::new();
    if toa & 0x70 == 0x10 {
        s.push('+')
    }
    for b in raw {
        for n in [b & 15, b >> 4] {
            if s.chars().filter(|c| c.is_ascii_digit()).count() >= digits {
                break;
            }
            if n <= 9 {
                s.push(char::from(b'0' + n))
            }
        }
    }
    s
}
fn decode_timestamp(raw: &[u8]) -> String {
    fn pair(x: u8) -> u8 {
        (x & 15) * 10 + (x >> 4)
    }
    if raw.len() != 7 {
        return String::new();
    }
    let negative = raw[6] & 0x08 != 0;
    let timezone = pair(raw[6] & !0x08);
    format!(
        "{:02}/{:02}/{:02},{:02}:{:02}:{:02}{}{:02}",
        pair(raw[0]),
        pair(raw[1]),
        pair(raw[2]),
        pair(raw[3]),
        pair(raw[4]),
        pair(raw[5]),
        if negative { '-' } else { '+' },
        timezone,
    )
}
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.bytes().all(|x| x.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
fn encode_hex(x: &[u8]) -> String {
    x.iter().map(|b| format!("{b:02X}")).collect()
}

fn decode_gsm7(data: &[u8], bit_offset: usize, count: usize) -> String {
    let mut out = String::new();
    let mut escape = false;
    for n in 0..count {
        let bit = bit_offset + n * 7;
        let byte = bit / 8;
        let shift = bit % 8;
        if byte >= data.len() {
            break;
        }
        let mut v = (data[byte] >> shift) & 0x7f;
        if shift > 1 && byte + 1 < data.len() {
            v |= data[byte + 1] << (8 - shift) & 0x7f
        }
        if escape {
            out.push(gsm7_ext(v));
            escape = false
        } else if v == 0x1b {
            escape = true
        } else {
            out.push(gsm7_char(v))
        }
    }
    if escape {
        out.push('\u{fffd}')
    }
    out
}
fn gsm7_ext(v: u8) -> char {
    match v {
        0x0a => '\u{000c}',
        0x14 => '^',
        0x28 => '{',
        0x29 => '}',
        0x2f => '\\',
        0x3c => '[',
        0x3d => '~',
        0x3e => ']',
        0x40 => '|',
        0x65 => '€',
        _ => '\u{fffd}',
    }
}
fn gsm7_char(v: u8) -> char {
    const T: [char; 128] = [
        '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', 'Δ', '_',
        'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1b}', 'Æ', 'æ', 'ß', 'É', ' ', '!', '"',
        '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2', '3', '4',
        '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '¡', 'A', 'B', 'C', 'D', 'E', 'F',
        'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
        'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§', '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j',
        'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö',
        'ñ', 'ü', 'à',
    ];
    T[v as usize]
}

/// Idempotent legacy normalization: only valid UTF-16BE hex is decoded.
pub fn decode_ucs2_body(body: &str, explicit_ucs2: bool) -> Option<String> {
    if !explicit_ucs2 {
        return None;
    }
    let bytes = decode_hex(body)?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|x| u16::from_be_bytes([x[0], x[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .ok()
        .map(|x| x.strip_prefix('\u{feff}').unwrap_or(&x).to_owned())
}

fn quoted_csv(input: &str) -> Vec<String> {
    let mut r = vec![];
    let mut v = String::new();
    let mut q = false;
    for c in input.chars() {
        match c {
            '"' => q = !q,
            ',' if !q => {
                r.push(v.trim().into());
                v.clear()
            }
            _ => v.push(c),
        }
    }
    r.push(v.trim().into());
    r
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
        Err(ModemError::Validation(
            "use an international number such as +66812345678".into(),
        ))
    } else {
        Ok(compact)
    }
}
pub fn validate_body(body: &str) -> Result<&'static str, ModemError> {
    if body.is_empty() {
        return Err(ModemError::Validation("message cannot be empty".into()));
    }
    let gsm = body.chars().all(|c| c.is_ascii() && !c.is_ascii_control());
    if (gsm && body.chars().count() <= 160) || (!gsm && body.encode_utf16().count() <= 70) {
        Ok(if gsm { "GSM-7" } else { "UCS2" })
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
    fn deliver(index: i32, dcs: u8, udh: &[u8], payload: &[u8], udl: usize) -> Vec<String> {
        let mut p = vec![
            0,
            if udh.is_empty() { 0 } else { 0x40 },
            3,
            0x81,
            0x91,
            0xf1,
            0,
            dcs,
        ];
        p.extend([0; 7]);
        p.push(udl as u8);
        p.extend(udh);
        p.extend(payload);
        vec![
            format!("+CMGL: {index},\"REC READ\",,{}", p.len() - 1),
            encode_hex(&p),
        ]
    }
    fn ucs2_part(
        index: i32,
        reference: u16,
        total: u8,
        sequence: u8,
        text: &str,
        sixteen: bool,
    ) -> Vec<String> {
        let udh = if sixteen {
            vec![
                6,
                8,
                4,
                (reference >> 8) as u8,
                reference as u8,
                total,
                sequence,
            ]
        } else {
            vec![5, 0, 3, reference as u8, total, sequence]
        };
        let payload: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        let udl = udh.len() + payload.len();
        deliver(index, 8, &udh, &payload, udl)
    }
    fn gsm_part(index: i32, reference: u8, total: u8, sequence: u8, septets: &[u8]) -> Vec<String> {
        let udh = vec![5, 0, 3, reference, total, sequence];
        let header_septets = (udh.len() * 8).div_ceil(7);
        let bit_offset = header_septets * 7;
        let mut packed = vec![0; (bit_offset + septets.len() * 7).div_ceil(8)];
        packed[..udh.len()].copy_from_slice(&udh);
        for (n, value) in septets.iter().enumerate() {
            for bit in 0..7 {
                if value & (1 << bit) != 0 {
                    let position = bit_offset + n * 7 + bit;
                    packed[position / 8] |= 1 << (position % 8)
                }
            }
        }
        deliver(
            index,
            0,
            &udh,
            &packed[udh.len()..],
            header_septets + septets.len(),
        )
    }
    #[test]
    fn normalization_preserves_decoded_whitespace() {
        assert_eq!(decode_ucs2_body("Xin  chao", true), None);
        assert_eq!(
            decode_ucs2_body("00580069006E00200020006300680061006F", true).unwrap(),
            "Xin  chao"
        )
    }
    #[test]
    fn validates() {
        assert!(normalize_number("+66812345678").is_ok());
        assert_eq!(ucs2_hex("Aก"), "00410E01")
    }
    #[test]
    fn malformed_pdu_is_lossless() {
        let x = parse_cmgl(&["+CMGL: 2,\"REC READ\",,4".into(), "XYZ".into(), "OK".into()]);
        assert_eq!(x[0].body, "[hex] XYZ")
    }
    #[test]
    fn pdu_mode_numeric_statuses_are_normalized() {
        let sent = parse_cmgl(&["+CMGL: 2,3,,4".into(), "XYZ".into()]);
        let unsent = parse_cmgl(&["+CMGL: 3,2,,4".into(), "XYZ".into()]);
        assert_eq!(sent[0].modem_status, "STO SENT");
        assert_eq!(unsent[0].modem_status, "STO UNSENT");
    }
    #[test]
    fn assembles_out_of_order_ucs2_without_changing_whitespace() {
        let mut lines = ucs2_part(8, 0x34, 2, 2, " chao", false);
        lines.extend(ucs2_part(7, 0x34, 2, 1, "Xin ", false));
        lines.push("OK".into());
        let x = parse_cmgl(&lines);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].body, "Xin  chao");
        assert_eq!(x[0].storage_indices, vec![7, 8]);
        assert!(x[0].multipart_complete);
    }
    #[test]
    fn supports_16_bit_refs_deduplicates_and_reports_missing_parts() {
        let mut lines = ucs2_part(4, 0x1234, 3, 1, "one", true);
        lines.extend(ucs2_part(5, 0x1234, 3, 1, "duplicate", true));
        lines.extend(ucs2_part(6, 0x1234, 3, 3, "three", true));
        let x = parse_cmgl(&lines);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].body, "onethree");
        assert_eq!(x[0].parts_received, 2);
        assert_eq!(x[0].part_count, 3);
        assert!(!x[0].multipart_complete);
    }
    #[test]
    fn adjacent_independent_messages_are_not_merged() {
        let mut lines = deliver(1, 8, &[], &[0, 65], 2);
        lines.extend(deliver(2, 8, &[], &[0, 66], 2));
        assert_eq!(
            parse_cmgl(&lines)
                .iter()
                .map(|x| x.body.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
    }
    #[test]
    fn gsm7_udh_alignment_and_extension_characters_are_exact() {
        let mut lines = gsm_part(12, 9, 2, 2, b"URL");
        lines.extend(gsm_part(11, 9, 2, 1, &[b'A', 0x1b, 0x14, b' ']));
        let x = parse_cmgl(&lines);
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].body, "A^ URL");
    }
}
