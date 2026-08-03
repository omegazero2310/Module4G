use crate::ModemError;
use std::time::Duration;

pub const AMR_NB_MAGIC: &[u8] = b"#!AMR\n";
pub const TRANSFER_CHUNK_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmrInfo {
    pub frames: u32,
    pub duration: Duration,
}

/// Validates an octet-aligned AMR-NB storage file and calculates its duration.
pub fn inspect_amr(data: &[u8], limit: usize) -> Result<AmrInfo, ModemError> {
    if data.len() > limit {
        return Err(ModemError::Validation(format!(
            "audio exceeds {limit} byte limit"
        )));
    }
    if !data.starts_with(AMR_NB_MAGIC) {
        return Err(ModemError::Validation(
            "audio must be an AMR-NB file with a #!AMR header".into(),
        ));
    }
    if data.len() == AMR_NB_MAGIC.len() {
        return Err(ModemError::Validation(
            "AMR-NB file must contain at least one frame".into(),
        ));
    }

    // Bytes after each frame header for FT 0..=8 and the no-data FT 15.
    const PAYLOAD_BYTES: [Option<usize>; 16] = [
        Some(12),
        Some(13),
        Some(15),
        Some(17),
        Some(19),
        Some(20),
        Some(26),
        Some(31),
        Some(5),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(0),
    ];
    let mut offset = AMR_NB_MAGIC.len();
    let mut frames = 0_u32;
    while offset < data.len() {
        let toc = data[offset];
        if toc & 0x83 != 0 {
            return Err(ModemError::Validation(format!(
                "invalid AMR-NB frame header at byte {offset}"
            )));
        }
        let frame_type = usize::from((toc >> 3) & 0x0f);
        let payload = PAYLOAD_BYTES[frame_type].ok_or_else(|| {
            ModemError::Validation(format!(
                "reserved AMR-NB frame type {frame_type} at byte {offset}"
            ))
        })?;
        let frame_end = offset.saturating_add(1).saturating_add(payload);
        if frame_end > data.len() {
            return Err(ModemError::Validation(format!(
                "truncated AMR-NB frame at byte {offset}"
            )));
        }
        offset = frame_end;
        frames = frames
            .checked_add(1)
            .ok_or_else(|| ModemError::Validation("AMR-NB file contains too many frames".into()))?;
    }
    Ok(AmrInfo {
        frames,
        duration: Duration::from_millis(u64::from(frames) * 20),
    })
}

pub fn validate_audio_name(name: &str) -> Result<String, ModemError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.len() > 255
        || trimmed.chars().any(char::is_control)
        || !trimmed.to_ascii_lowercase().ends_with(".amr")
    {
        return Err(ModemError::Validation(
            "select a file with an .amr extension".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

pub fn module_path(audio_id: &str) -> Result<String, ModemError> {
    if audio_id.is_empty()
        || audio_id.len() > 64
        || !audio_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(ModemError::Validation("invalid audio identifier".into()));
    }
    Ok(format!("c:/call_{audio_id}.amr"))
}

pub fn playback_deadline(duration: Duration) -> Duration {
    duration + Duration::from_secs(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(frame_type: u8, payload_len: usize) -> Vec<u8> {
        let mut value = vec![frame_type << 3 | 0x04];
        value.extend(vec![0x55; payload_len]);
        value
    }

    #[test]
    fn validates_frames_and_calculates_duration() {
        let mut data = AMR_NB_MAGIC.to_vec();
        data.extend(frame(0, 12));
        data.extend(frame(8, 5));
        let info = inspect_amr(&data, data.len()).unwrap();
        assert_eq!(info.frames, 2);
        assert_eq!(info.duration, Duration::from_millis(40));
    }

    #[test]
    fn rejects_empty_oversized_reserved_and_truncated_files() {
        assert!(inspect_amr(AMR_NB_MAGIC, 100).is_err());
        assert!(inspect_amr(b"#!AMR\n\x04", 6).is_err());
        assert!(inspect_amr(b"#!AMR\n\x4c", 100).is_err());
        assert!(inspect_amr(b"#!AMR\n\x04\x00", 100).is_err());
    }

    #[test]
    fn creates_only_safe_module_paths() {
        assert_eq!(
            module_path("01ABC_xyz-9").unwrap(),
            "c:/call_01ABC_xyz-9.amr"
        );
        assert!(module_path("../bad").is_err());
        assert!(module_path("bad\"").is_err());
    }
}
