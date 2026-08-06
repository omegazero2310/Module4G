use super::*;

pub(super) fn result_confirms_liveness(result: &Result<Vec<String>, ModemError>) -> bool {
    matches!(result, Ok(_) | Err(ModemError::CommandRejected(_)))
}

pub(super) fn result_requires_reconnect(result: &Result<Vec<String>, ModemError>) -> bool {
    matches!(
        result,
        Err(ModemError::Disconnected
            | ModemError::Timeout
            | ModemError::SmsSubmitTimeout { .. }
            | ModemError::RawUploadTimeout { .. })
    )
}

pub(super) fn publish_sms_events(dispatcher: &mut Dispatcher, sender: &mpsc::Sender<SmsUrcEvent>) {
    for event in dispatcher.take_complete_sms_urcs() {
        let typed = if event.first().is_some_and(|line| line.starts_with("+CDS:")) {
            SmsUrcEvent::DirectReport(event)
        } else if let Some(line) = event.into_iter().next() {
            SmsUrcEvent::StoredIndication(line)
        } else {
            continue;
        };
        let _ = sender.send(typed);
    }
    // Complete events have their own owned lines; this legacy buffer is only
    // retained for callers/tests that explicitly inspect it.
    let _ = dispatcher.take_sms_urcs();
}

pub(super) fn run_batch<F>(
    commands: &[String],
    finalizer: Option<&str>,
    mut run: F,
) -> Result<Vec<String>, ModemError>
where
    F: FnMut(&str) -> Result<Vec<String>, ModemError>,
{
    let mut result = Ok(Vec::new());
    for command in commands {
        match run(command) {
            Ok(lines) => result = Ok(lines),
            Err(error) => {
                result = Err(error);
                break;
            }
        }
    }
    if let Some(command) = finalizer {
        let restored = run(command);
        if result.is_ok() {
            if let Err(error) = restored {
                result = Err(error);
            }
        }
    }
    result
}

pub(super) fn execute_command(
    port: &mut dyn SerialPort,
    command: &str,
    payload: Option<&[u8]>,
    guarded: bool,
    timeout: Duration,
    payload_mode: PayloadMode,
    dispatcher: &mut Dispatcher,
) -> Result<Vec<String>, ModemError> {
    if guarded {
        crate::at::validate_console(command, false)?;
    }
    if command.starts_with("ATD") {
        dispatcher.clear_urcs();
    }
    if let (Some(payload), PayloadMode::Raw { pacing }) = (payload, payload_mode) {
        return execute_raw_upload(
            port,
            command,
            payload,
            pacing,
            RawUploadDeadlines::default(),
            dispatcher,
        );
    }
    if let (Some(payload), PayloadMode::Sms) = (payload, payload_mode) {
        return execute_sms_submit(port, command, payload, dispatcher);
    }
    port.write_all(command.as_bytes())
        .map_err(|_| ModemError::Disconnected)?;
    port.write_all(b"\r")
        .map_err(|_| ModemError::Disconnected)?;
    port.flush().map_err(|_| ModemError::Disconnected)?;
    let deadline = Instant::now() + timeout;
    let mut lines = Vec::new();
    let mut buffer = [0_u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                let (frames, _) = dispatcher.push(&buffer[..count], Some(command));
                if command.eq_ignore_ascii_case("AT+CLCC") {
                    lines.extend(dispatcher.take_urcs());
                }
                for frame in frames {
                    if let Frame::Line(line) = &frame {
                        if line.eq_ignore_ascii_case(command) {
                            continue;
                        }
                        if line == "OK" {
                            if command.eq_ignore_ascii_case("AT+CMGL=4") {
                                lines.extend(dispatcher.take_sms_urcs().into_iter().filter(
                                    |line| {
                                        line.starts_with("+CDS:")
                                            || line.bytes().all(|byte| byte.is_ascii_hexdigit())
                                    },
                                ));
                            }
                            return Ok(lines);
                        }
                        if line == "ERROR"
                            || line.starts_with("+CME ERROR:")
                            || line.starts_with("+CMS ERROR:")
                        {
                            return Err(ModemError::CommandRejected(line.clone()));
                        }
                        lines.push(line.clone());
                    } else if matches!(frame, Frame::Prompt) {
                        let Some(payload) = payload else {
                            return Err(ModemError::Validation(
                                "modem requested an unexpected payload".into(),
                            ));
                        };
                        match payload_mode {
                            PayloadMode::Sms => {
                                port.write_all(payload)
                                    .map_err(|_| ModemError::Disconnected)?;
                                port.write_all(&[0x1a])
                                    .map_err(|_| ModemError::Disconnected)?;
                            }
                            PayloadMode::Raw { .. } => {
                                unreachable!("raw uploads use phased transfer")
                            }
                        }
                        port.flush().map_err(|_| ModemError::Disconnected)?;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => return Err(ModemError::Disconnected),
        }
    }
    Err(ModemError::Timeout)
}
