use crate::ModemError;

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
}
