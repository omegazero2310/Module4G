use crate::ModemError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub usb_vid: u16,
    pub usb_pid: u16,
    pub port_override: Option<String>,
    pub baud: u32,
    pub call_timeout_seconds: u32,
    #[serde(default = "default_voicemail_guard_seconds")]
    pub voicemail_guard_seconds: u32,
    pub upload_pacing_ms: u32,
    pub max_audio_bytes: usize,
    pub ussd_code: String,
    pub ussd_timeout_seconds: u32,
    pub currency: String,
    pub low_balance_threshold: f64,
    pub balance_regex: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            usb_vid: 0x1e0e,
            usb_pid: 0x9011,
            port_override: None,
            baud: 115_200,
            call_timeout_seconds: 90,
            voicemail_guard_seconds: default_voicemail_guard_seconds(),
            upload_pacing_ms: 10,
            max_audio_bytes: 200 * 1024,
            ussd_code: "*101#".into(),
            ussd_timeout_seconds: 30,
            currency: String::new(),
            low_balance_threshold: 0.0,
            balance_regex: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), ModemError> {
        if self.usb_vid == 0 || self.usb_pid == 0 {
            return Err(ModemError::Validation(
                "USB VID and PID must be non-zero".into(),
            ));
        }
        if let Some(port) = &self.port_override {
            let upper = port.trim().to_ascii_uppercase();
            if !upper.starts_with("COM") || upper[3..].parse::<u16>().is_err() {
                return Err(ModemError::Validation(
                    "port override must look like COM6".into(),
                ));
            }
        }
        if !(1_200..=921_600).contains(&self.baud) {
            return Err(ModemError::Validation(
                "baud must be between 1200 and 921600".into(),
            ));
        }
        if self.max_audio_bytes == 0 || self.max_audio_bytes > 1024 * 1024 {
            return Err(ModemError::Validation(
                "audio limit must be between 1 byte and 1 MiB".into(),
            ));
        }
        if self.ussd_timeout_seconds == 0 || self.call_timeout_seconds == 0 {
            return Err(ModemError::Validation("timeouts must be positive".into()));
        }
        if !(5..=60).contains(&self.voicemail_guard_seconds)
            || self.voicemail_guard_seconds >= self.call_timeout_seconds
        {
            return Err(ModemError::Validation(
                "voicemail guard must be 5 to 60 seconds and below the call timeout".into(),
            ));
        }
        if self.call_timeout_seconds > 600 || self.ussd_timeout_seconds > 300 {
            return Err(ModemError::Validation(
                "timeouts exceed the supported maximum".into(),
            ));
        }
        if self.upload_pacing_ms > 5_000 {
            return Err(ModemError::Validation(
                "upload pacing must not exceed 5000 ms".into(),
            ));
        }
        if self.ussd_code.is_empty()
            || self.ussd_code.len() > 64
            || self.ussd_code.chars().any(char::is_control)
        {
            return Err(ModemError::Validation(
                "USSD code must contain 1 to 64 printable characters".into(),
            ));
        }
        if self.currency.len() > 12 || self.currency.chars().any(char::is_control) {
            return Err(ModemError::Validation(
                "currency must be at most 12 printable characters".into(),
            ));
        }
        if !self.low_balance_threshold.is_finite() || self.low_balance_threshold < 0.0 {
            return Err(ModemError::Validation(
                "low balance threshold must be a non-negative number".into(),
            ));
        }
        if self
            .balance_regex
            .as_ref()
            .is_some_and(|value| value.len() > 512)
        {
            return Err(ModemError::Validation(
                "balance regex must be at most 512 characters".into(),
            ));
        }
        Ok(())
    }
}

const fn default_voicemail_guard_seconds() -> u32 {
    15
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_every_user_editable_boundary() {
        assert!(Settings::default().validate().is_ok());
        let mut settings = Settings::default();
        settings.port_override = Some("not-a-port".into());
        assert!(settings.validate().is_err());
        settings.port_override = Some("COM6".into());
        settings.low_balance_threshold = f64::NAN;
        assert!(settings.validate().is_err());
    }
}
