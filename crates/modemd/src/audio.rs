use crate::ModemError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Amr,
    Wav,
}

pub fn inspect_audio(data: &[u8], limit: usize) -> Result<AudioFormat, ModemError> {
    if data.len() > limit {
        return Err(ModemError::Validation(format!(
            "audio exceeds {limit} byte limit"
        )));
    }
    if data.starts_with(b"#!AMR\n") {
        return Ok(AudioFormat::Amr);
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return Ok(AudioFormat::Wav);
    }
    Err(ModemError::Validation(
        "only valid AMR and WAV files are accepted".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_signatures() {
        assert_eq!(inspect_audio(b"#!AMR\nx", 100), Ok(AudioFormat::Amr));
        assert_eq!(inspect_audio(b"RIFF0000WAVE", 100), Ok(AudioFormat::Wav));
        assert!(inspect_audio(b"fake.wav", 100).is_err());
    }
}
