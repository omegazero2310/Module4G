use super::*;

pub(super) fn execute_sms_submit(
    port: &mut dyn SerialPort,
    command: &str,
    payload: &[u8],
    dispatcher: &mut Dispatcher,
) -> Result<Vec<String>, ModemError> {
    execute_sms_submit_with_deadlines(
        port,
        command,
        payload,
        dispatcher,
        SMS_PROMPT_TIMEOUT,
        SMS_SUBMIT_TIMEOUT,
        INTERACTIVE_RESYNC_WINDOW,
    )
}

pub(super) fn execute_sms_submit_with_deadlines(
    port: &mut dyn SerialPort,
    command: &str,
    payload: &[u8],
    dispatcher: &mut Dispatcher,
    prompt_timeout: Duration,
    result_timeout: Duration,
    resync_window: Duration,
) -> Result<Vec<String>, ModemError> {
    let started = Instant::now();
    port.write_all(command.as_bytes())
        .map_err(|_| ModemError::Disconnected)?;
    port.write_all(b"\r")
        .map_err(|_| ModemError::Disconnected)?;
    port.flush().map_err(|_| ModemError::Disconnected)?;
    let mut buffer = [0_u8; 256];
    let prompt_deadline = Instant::now() + prompt_timeout;
    loop {
        if Instant::now() >= prompt_deadline {
            let resynchronized = resynchronize_interactive(port, dispatcher, resync_window);
            eprintln!(
                "sms_submit_phase=prompt status=timeout elapsed_ms={} resynchronized={resynchronized}",
                started.elapsed().as_millis()
            );
            return Err(ModemError::SmsSubmitTimeout {
                phase: "before the payload prompt",
                resynchronized,
            });
        }
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                let (frames, _) = dispatcher.push(&buffer[..count], Some(command));
                let mut prompted = false;
                for frame in frames {
                    match frame {
                        Frame::Prompt => prompted = true,
                        Frame::Line(line) if is_rejection(&line) => {
                            return Err(ModemError::CommandRejected(line));
                        }
                        _ => {}
                    }
                }
                if prompted {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => return Err(ModemError::Disconnected),
        }
    }
    port.write_all(payload)
        .map_err(|_| ModemError::Disconnected)?;
    port.write_all(&[0x1a])
        .map_err(|_| ModemError::Disconnected)?;
    port.flush().map_err(|_| ModemError::Disconnected)?;
    eprintln!(
        "sms_submit_phase=payload status=sent bytes={} elapsed_ms={}",
        payload.len(),
        started.elapsed().as_millis()
    );

    let result_deadline = Instant::now() + result_timeout;
    let mut lines = Vec::new();
    let mut got_reference = false;
    let mut got_ok = false;
    loop {
        if got_reference && got_ok {
            eprintln!(
                "sms_submit_phase=result status=accepted elapsed_ms={}",
                started.elapsed().as_millis()
            );
            return Ok(lines);
        }
        if Instant::now() >= result_deadline {
            let resynchronized = resynchronize_interactive(port, dispatcher, resync_window);
            eprintln!(
                "sms_submit_phase=result status=timeout elapsed_ms={} resynchronized={resynchronized}",
                started.elapsed().as_millis()
            );
            return Err(ModemError::SmsSubmitTimeout {
                phase: "after payload transmission",
                resynchronized,
            });
        }
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                let (frames, _) = dispatcher.push(&buffer[..count], Some(command));
                for frame in frames {
                    if let Frame::Line(line) = frame {
                        if is_rejection(&line) {
                            return Err(ModemError::CommandRejected(line));
                        }
                        if line.starts_with("+CMGS:") {
                            got_reference = true;
                            lines.push(line);
                        } else if line == "OK" {
                            got_ok = true;
                        } else {
                            lines.push(line);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => return Err(ModemError::Disconnected),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RawUploadDeadlines {
    pub(super) prompt: Duration,
    pub(super) final_result: Duration,
    pub(super) resync: Duration,
}

impl Default for RawUploadDeadlines {
    fn default() -> Self {
        Self {
            prompt: RAW_PROMPT_TIMEOUT,
            final_result: RAW_RESULT_TIMEOUT,
            resync: INTERACTIVE_RESYNC_WINDOW,
        }
    }
}

pub(super) fn execute_raw_upload(
    port: &mut dyn SerialPort,
    command: &str,
    payload: &[u8],
    pacing: Duration,
    deadlines: RawUploadDeadlines,
    dispatcher: &mut Dispatcher,
) -> Result<Vec<String>, ModemError> {
    let started = Instant::now();
    port.write_all(command.as_bytes())
        .map_err(|_| ModemError::Disconnected)?;
    port.write_all(b"\r")
        .map_err(|_| ModemError::Disconnected)?;
    port.flush().map_err(|_| ModemError::Disconnected)?;
    eprintln!(
        "upload_phase=command status=sent declared_bytes={} elapsed_ms={}",
        payload.len(),
        started.elapsed().as_millis()
    );

    let mut lines = Vec::new();
    let prompt_deadline = Instant::now() + deadlines.prompt;
    let mut buffer = [0_u8; 256];
    loop {
        if Instant::now() >= prompt_deadline {
            let resynchronized = resynchronize_interactive(port, dispatcher, deadlines.resync);
            eprintln!(
                "upload_phase=prompt status=timeout elapsed_ms={} resynchronized={resynchronized}",
                started.elapsed().as_millis()
            );
            return Err(raw_upload_timeout(
                RawUploadTimeoutPhase::Prompt,
                0,
                0,
                pacing,
                started.elapsed(),
                resynchronized,
            ));
        }
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                let (frames, _) = dispatcher.push(&buffer[..count], Some(command));
                let mut prompted = false;
                for frame in frames {
                    match frame {
                        Frame::Prompt => prompted = true,
                        Frame::Line(line) if is_rejection(&line) => {
                            eprintln!(
                                "upload_phase=prompt status=rejected elapsed_ms={}",
                                started.elapsed().as_millis()
                            );
                            return Err(ModemError::CommandRejected(line));
                        }
                        Frame::Line(line) if line != "OK" => lines.push(line),
                        Frame::Line(_) => {}
                    }
                }
                if prompted {
                    eprintln!(
                        "upload_phase=prompt status=received elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => return Err(ModemError::Disconnected),
        }
    }

    let chunks = payload.len().div_ceil(crate::audio::TRANSFER_CHUNK_BYTES);
    transfer_raw_payload(payload, pacing, |chunk| {
        port.write_all(chunk).map_err(|_| ModemError::Disconnected)
    })?;
    port.flush().map_err(|_| ModemError::Disconnected)?;
    eprintln!(
        "upload_phase=payload status=sent bytes={} chunks={} chunk_bytes={} pacing_ms={} elapsed_ms={}",
        payload.len(),
        chunks,
        crate::audio::TRANSFER_CHUNK_BYTES,
        pacing.as_millis(),
        started.elapsed().as_millis()
    );

    let result_deadline = Instant::now() + deadlines.final_result;
    loop {
        if Instant::now() >= result_deadline {
            let resynchronized = resynchronize_interactive(port, dispatcher, deadlines.resync);
            eprintln!(
                "upload_phase=result status=timeout bytes={} chunks={} pacing_ms={} elapsed_ms={} resynchronized={resynchronized}",
                payload.len(),
                chunks,
                pacing.as_millis(),
                started.elapsed().as_millis()
            );
            return Err(raw_upload_timeout(
                RawUploadTimeoutPhase::FinalResult,
                payload.len(),
                chunks,
                pacing,
                started.elapsed(),
                resynchronized,
            ));
        }
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                let (frames, _) = dispatcher.push(&buffer[..count], Some(command));
                for frame in frames {
                    if let Frame::Line(line) = frame {
                        if line == "OK" {
                            eprintln!(
                                "upload_phase=result status=ok bytes={} chunks={} pacing_ms={} elapsed_ms={}",
                                payload.len(),
                                chunks,
                                pacing.as_millis(),
                                started.elapsed().as_millis()
                            );
                            return Ok(lines);
                        }
                        if is_rejection(&line) {
                            eprintln!(
                                "upload_phase=result status=rejected bytes={} chunks={} pacing_ms={} elapsed_ms={}",
                                payload.len(),
                                chunks,
                                pacing.as_millis(),
                                started.elapsed().as_millis()
                            );
                            return Err(ModemError::CommandRejected(line));
                        }
                        lines.push(line);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => return Err(ModemError::Disconnected),
        }
    }
}

pub(super) fn is_rejection(line: &str) -> bool {
    line == "ERROR" || line.starts_with("+CME ERROR:") || line.starts_with("+CMS ERROR:")
}

pub(super) fn raw_upload_timeout(
    phase: RawUploadTimeoutPhase,
    bytes_sent: usize,
    chunks_sent: usize,
    pacing: Duration,
    elapsed: Duration,
    resynchronized: bool,
) -> ModemError {
    ModemError::RawUploadTimeout {
        phase,
        bytes_sent,
        chunks_sent,
        pacing_ms: pacing.as_millis().try_into().unwrap_or(u64::MAX),
        elapsed_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        resynchronized,
    }
}

pub(super) fn resynchronize_interactive(
    port: &mut dyn SerialPort,
    dispatcher: &mut Dispatcher,
    window: Duration,
) -> bool {
    dispatcher.reset();
    if port.clear(serialport::ClearBuffer::Input).is_err() {
        return false;
    }
    let deadline = Instant::now() + window;
    let mut buffer = [0_u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buffer) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::TimedOut => thread::yield_now(),
            Err(_) => return false,
        }
    }
    let cleared = port.clear(serialport::ClearBuffer::Input).is_ok();
    dispatcher.reset();
    cleared
}

pub(super) fn payload_timeout(payload: Option<&[u8]>, mode: PayloadMode) -> Duration {
    match (payload, mode) {
        (Some(data), PayloadMode::Raw { pacing }) => {
            let chunks = data.len().div_ceil(crate::audio::TRANSFER_CHUNK_BYTES) as u32;
            SMS_SUBMIT_TIMEOUT.saturating_add(pacing.saturating_mul(chunks))
        }
        (Some(_), PayloadMode::Sms) => SMS_SUBMIT_TIMEOUT,
        (None, _) => COMMAND_TIMEOUT,
    }
}

pub(super) fn command_timeout(command: &str) -> Duration {
    if [
        "AT+CMGF", "AT+CPMS", "AT+CSMP", "AT+CNMI", "AT+CMGL", "AT+CMGD",
    ]
    .iter()
    .any(|prefix| command.to_ascii_uppercase().starts_with(prefix))
    {
        SMS_STORAGE_COMMAND_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    }
}

pub(super) fn transfer_raw_payload(
    payload: &[u8],
    pacing: Duration,
    mut write: impl FnMut(&[u8]) -> Result<(), ModemError>,
) -> Result<(), ModemError> {
    let count = payload.len().div_ceil(crate::audio::TRANSFER_CHUNK_BYTES);
    for (index, chunk) in payload
        .chunks(crate::audio::TRANSFER_CHUNK_BYTES)
        .enumerate()
    {
        write(chunk)?;
        if index + 1 < count && !pacing.is_zero() {
            thread::sleep(pacing);
        }
    }
    Ok(())
}
