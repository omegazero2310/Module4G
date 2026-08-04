use crate::{
    ModemError, RawUploadTimeoutPhase,
    at::{Dispatcher, Frame, Framer},
    settings::Settings,
};
use serialport::{SerialPort, SerialPortType};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const SMS_SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);
const RAW_PROMPT_TIMEOUT: Duration = Duration::from_secs(2);
const RAW_RESULT_TIMEOUT: Duration = Duration::from_secs(30);
const INTERACTIVE_RESYNC_WINDOW: Duration = Duration::from_millis(300);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const INITIALIZATION_COMMANDS: &[&str] = &["AT+CMEE=2", "AT+CVHU=0", "AT+CMGF=1"];
const OPTIONAL_INITIALIZATION_COMMANDS: &[&str] = &[
    "AT+CLCC=1",
    "AT+CNMI=2,1,0,1,0",
    // TP-SRR requests a network delivery report for SMS-SUBMIT messages.
    "AT+CSMP=49,167,0,0",
    "AT+CSDH=1",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortCandidate {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub serial_number: Option<String>,
    pub product: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum HardwareError {
    #[error("could not enumerate serial ports: {0}")]
    Enumeration(#[source] serialport::Error),
    #[error("no serial ports matched USB device {vid:04x}:{pid:04x}")]
    NoMatchingDevice { vid: u16, pid: u16 },
    #[error("no matching port responded to an AT probe")]
    NoAtPort,
    #[error("{port_name} is present but cannot be opened; another application may be using it")]
    PortBusy {
        port_name: String,
        #[source]
        source: serialport::Error,
    },
    #[error("serial I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("modem rejected {command}: {response}")]
    CommandRejected { command: String, response: String },
    #[error(transparent)]
    Modem(#[from] ModemError),
}

pub struct InitializedModem {
    pub port_name: String,
    pub serial_number: Option<String>,
    port: Box<dyn SerialPort>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HardwareState {
    Disconnected,
    PortBusy { port_name: String },
    Ready { port_name: String },
}

pub struct AtRequest {
    pub command: String,
    pub payload: Option<Vec<u8>>,
    pub guarded: bool,
    pub payload_mode: PayloadMode,
    /// Commands executed under one actor dequeue, preventing mode changes from
    /// interleaving with unrelated requests. The finalizer is always attempted.
    pub batch: Vec<String>,
    pub finalizer: Option<String>,
    pub reply: tokio::sync::oneshot::Sender<Result<Vec<String>, ModemError>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PayloadMode {
    #[default]
    Sms,
    Raw {
        pacing: Duration,
    },
}

impl InitializedModem {
    pub fn port(&mut self) -> &mut dyn SerialPort {
        self.port.as_mut()
    }

    fn is_present(&self) -> bool {
        self.port.bytes_to_read().is_ok()
            && serialport::available_ports().is_ok_and(|ports| {
                ports
                    .iter()
                    .any(|port| port.port_name.eq_ignore_ascii_case(&self.port_name))
            })
    }
}

/// Continuously discovers the modem and reports plug/unplug state changes.
///
/// The returned serial handle stays owned by this loop until removal, ensuring
/// no second process can claim the AT port between initialization and use.
pub fn monitor(settings: Settings, stop: Arc<AtomicBool>, mut report: impl FnMut(HardwareState)) {
    let (_sender, receiver) = mpsc::channel();
    monitor_with_commands(settings, stop, &mut report, receiver);
}

/// Owns the serial port and executes guarded console requests sequentially.
pub fn monitor_with_commands(
    settings: Settings,
    stop: Arc<AtomicBool>,
    mut report: impl FnMut(HardwareState),
    commands: mpsc::Receiver<AtRequest>,
) {
    let mut modem: Option<InitializedModem> = None;
    let mut dispatcher = Dispatcher::default();
    let mut last_state: Option<HardwareState> = None;
    while !stop.load(Ordering::Relaxed) {
        if modem.as_ref().is_some_and(InitializedModem::is_present) {
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(request) => {
                    let port = modem.as_mut().expect("presence checked").port();
                    let result = if request.batch.is_empty() {
                        execute_command(
                            port,
                            &request.command,
                            request.payload.as_deref(),
                            request.guarded,
                            payload_timeout(request.payload.as_deref(), request.payload_mode),
                            request.payload_mode,
                            &mut dispatcher,
                        )
                    } else {
                        run_batch(&request.batch, request.finalizer.as_deref(), |command| {
                            execute_command(
                                port,
                                command,
                                None,
                                false,
                                COMMAND_TIMEOUT,
                                PayloadMode::Sms,
                                &mut dispatcher,
                            )
                        })
                    };
                    let _ = request.reply.send(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(DEVICE_POLL_INTERVAL),
            }
            continue;
        }

        modem = None;
        match discover_and_initialize(&settings) {
            Ok(connected) => {
                let state = HardwareState::Ready {
                    port_name: connected.port_name.clone(),
                };
                modem = Some(connected);
                publish_if_changed(&mut last_state, state, &mut report);
            }
            Err(HardwareError::PortBusy { port_name, .. }) => publish_if_changed(
                &mut last_state,
                HardwareState::PortBusy { port_name },
                &mut report,
            ),
            Err(_) => publish_if_changed(&mut last_state, HardwareState::Disconnected, &mut report),
        }
        thread::sleep(DEVICE_POLL_INTERVAL);
    }
}

fn run_batch<F>(
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

fn execute_command(
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

#[derive(Clone, Copy)]
struct RawUploadDeadlines {
    prompt: Duration,
    final_result: Duration,
    resync: Duration,
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

fn execute_raw_upload(
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

fn is_rejection(line: &str) -> bool {
    line == "ERROR" || line.starts_with("+CME ERROR:") || line.starts_with("+CMS ERROR:")
}

fn raw_upload_timeout(
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

fn resynchronize_interactive(
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

fn payload_timeout(payload: Option<&[u8]>, mode: PayloadMode) -> Duration {
    match (payload, mode) {
        (Some(data), PayloadMode::Raw { pacing }) => {
            let chunks = data.len().div_ceil(crate::audio::TRANSFER_CHUNK_BYTES) as u32;
            SMS_SUBMIT_TIMEOUT.saturating_add(pacing.saturating_mul(chunks))
        }
        (Some(_), PayloadMode::Sms) => SMS_SUBMIT_TIMEOUT,
        (None, _) => COMMAND_TIMEOUT,
    }
}

fn transfer_raw_payload(
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

fn publish_if_changed(
    previous: &mut Option<HardwareState>,
    state: HardwareState,
    report: &mut impl FnMut(HardwareState),
) {
    if previous.as_ref() != Some(&state) {
        *previous = Some(state.clone());
        report(state);
    }
}

pub fn enumerate(settings: &Settings) -> Result<Vec<PortCandidate>, HardwareError> {
    let ports = serialport::available_ports().map_err(HardwareError::Enumeration)?;
    let mut candidates: Vec<_> = ports
        .into_iter()
        .filter_map(|port| {
            if settings
                .port_override
                .as_deref()
                .is_some_and(|override_name| override_name.eq_ignore_ascii_case(&port.port_name))
            {
                return Some(PortCandidate {
                    name: port.port_name,
                    vid: settings.usb_vid,
                    pid: settings.usb_pid,
                    serial_number: None,
                    product: None,
                });
            }
            match port.port_type {
                SerialPortType::UsbPort(info)
                    if info.vid == settings.usb_vid && info.pid == settings.usb_pid =>
                {
                    Some(PortCandidate {
                        name: port.port_name,
                        vid: info.vid,
                        pid: info.pid,
                        serial_number: info.serial_number,
                        product: info.product,
                    })
                }
                _ => None,
            }
        })
        .collect();
    #[cfg(windows)]
    for candidate in windows_registry_candidates(settings) {
        if !candidates
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&candidate.name))
        {
            candidates.push(candidate);
        }
    }
    candidates.sort_by_key(candidate_sort_key);
    if candidates.is_empty() {
        Err(HardwareError::NoMatchingDevice {
            vid: settings.usb_vid,
            pid: settings.usb_pid,
        })
    } else {
        Ok(candidates)
    }
}

#[cfg(windows)]
fn windows_registry_candidates(settings: &Settings) -> Vec<PortCandidate> {
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

    const ACTIVE_PORTS: &str = r"HARDWARE\DEVICEMAP\SERIALCOMM";
    const DEVICE_PATHS: &str = r"SYSTEM\CurrentControlSet\Control\COM Name Arbiter\Devices";

    let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(active_ports) = machine.open_subkey(ACTIVE_PORTS) else {
        return Vec::new();
    };
    let Ok(device_paths) = machine.open_subkey(DEVICE_PATHS) else {
        return Vec::new();
    };
    let identity = format!("vid_{:04x}&pid_{:04x}", settings.usb_vid, settings.usb_pid);
    active_ports
        .enum_values()
        .filter_map(Result::ok)
        .filter_map(|(value_name, _)| active_ports.get_value::<String, _>(&value_name).ok())
        .filter_map(|port_name| {
            let path: String = device_paths.get_value(&port_name).ok()?;
            let path_lower = path.to_ascii_lowercase();
            if !path_lower.contains(&identity) {
                return None;
            }
            Some(PortCandidate {
                name: port_name,
                vid: settings.usb_vid,
                pid: settings.usb_pid,
                serial_number: None,
                product: path_lower.contains("&mi_04#").then(|| "AT Port".into()),
            })
        })
        .collect()
}

pub fn discover_and_initialize(settings: &Settings) -> Result<InitializedModem, HardwareError> {
    settings.validate()?;
    let candidates = enumerate(settings)?;
    for candidate in candidates {
        let opened = serialport::new(&candidate.name, settings.baud)
            .timeout(Duration::from_millis(100))
            .open();
        let mut port = match opened {
            Ok(port) => port,
            Err(source) if is_dedicated_at_port(&candidate) => {
                return Err(HardwareError::PortBusy {
                    port_name: candidate.name,
                    source,
                });
            }
            Err(_) => continue,
        };
        let _ = port.clear(serialport::ClearBuffer::Input);
        if send_expect_ok(port.as_mut(), "AT", PROBE_TIMEOUT).is_err() {
            continue;
        }
        initialize(port.as_mut())?;
        return Ok(InitializedModem {
            port_name: candidate.name,
            serial_number: candidate.serial_number,
            port,
        });
    }
    Err(HardwareError::NoAtPort)
}

fn initialize(port: &mut dyn SerialPort) -> Result<(), HardwareError> {
    for command in INITIALIZATION_COMMANDS {
        send_expect_ok(port, command, COMMAND_TIMEOUT)?;
    }
    for command in OPTIONAL_INITIALIZATION_COMMANDS {
        let _ = send_expect_ok(port, command, COMMAND_TIMEOUT);
    }
    Ok(())
}

fn send_expect_ok(
    port: &mut dyn SerialPort,
    command: &str,
    timeout: Duration,
) -> Result<(), HardwareError> {
    port.write_all(command.as_bytes())?;
    port.write_all(b"\r")?;
    port.flush()?;

    let deadline = Instant::now() + timeout;
    let mut framer = Framer::default();
    let mut response = Vec::new();
    let mut buffer = [0_u8; 256];
    while Instant::now() < deadline {
        match port.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => {
                for frame in framer.push(&buffer[..count]) {
                    if let Frame::Line(line) = frame {
                        if line == "OK" {
                            return Ok(());
                        }
                        if line == "ERROR"
                            || line.starts_with("+CME ERROR:")
                            || line.starts_with("+CMS ERROR:")
                        {
                            response.push(line);
                            return Err(HardwareError::CommandRejected {
                                command: command.into(),
                                response: response.join(" | "),
                            });
                        }
                        if !line.eq_ignore_ascii_case(command) {
                            response.push(line);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(HardwareError::Modem(ModemError::Timeout))
}

fn natural_port_key(name: &str) -> (String, u32) {
    let split = name
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(name.len());
    let (prefix, suffix) = name.split_at(split);
    (
        prefix.to_ascii_uppercase(),
        suffix.parse().unwrap_or(u32::MAX),
    )
}

fn candidate_sort_key(candidate: &PortCandidate) -> (bool, (String, u32)) {
    (
        !is_dedicated_at_port(candidate),
        natural_port_key(&candidate.name),
    )
}

fn is_dedicated_at_port(candidate: &PortCandidate) -> bool {
    candidate
        .product
        .as_deref()
        .is_some_and(|product| product.to_ascii_lowercase().contains("at port"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialport::{ClearBuffer, DataBits, FlowControl, Parity, StopBits};
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        sync::atomic::AtomicUsize,
    };

    enum ReadStep {
        Bytes(Vec<u8>),
        Timeout,
    }

    struct ScriptedPort {
        reads: VecDeque<ReadStep>,
        writes: Vec<Vec<u8>>,
        timeout: Duration,
        clears: AtomicUsize,
    }

    impl ScriptedPort {
        fn new(reads: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                writes: Vec::new(),
                timeout: Duration::from_millis(1),
                clears: AtomicUsize::new(0),
            }
        }

        fn serial_error() -> serialport::Error {
            serialport::Error::new(serialport::ErrorKind::Unknown, "unsupported in test")
        }
    }

    impl Read for ScriptedPort {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.reads.pop_front().unwrap_or(ReadStep::Timeout) {
                ReadStep::Bytes(bytes) => {
                    assert!(bytes.len() <= buffer.len());
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                ReadStep::Timeout => {
                    thread::sleep(self.timeout);
                    Err(io::ErrorKind::TimedOut.into())
                }
            }
        }
    }

    impl Write for ScriptedPort {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for ScriptedPort {
        fn name(&self) -> Option<String> {
            Some("scripted".into())
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(115_200)
        }
        fn data_bits(&self) -> serialport::Result<DataBits> {
            Ok(DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<FlowControl> {
            Ok(FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<Parity> {
            Ok(Parity::None)
        }
        fn stop_bits(&self) -> serialport::Result<StopBits> {
            Ok(StopBits::One)
        }
        fn timeout(&self) -> Duration {
            self.timeout
        }
        fn set_baud_rate(&mut self, _baud_rate: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _data_bits: DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _flow_control: FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _parity: Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _stop_bits: StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
            self.timeout = timeout;
            Ok(())
        }
        fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(true)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _buffer_to_clear: ClearBuffer) -> serialport::Result<()> {
            self.clears.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(Self::serial_error())
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    fn test_deadlines() -> RawUploadDeadlines {
        RawUploadDeadlines {
            prompt: Duration::from_millis(8),
            final_result: Duration::from_millis(8),
            resync: Duration::ZERO,
        }
    }

    fn raw_command(port: &mut ScriptedPort, payload: &[u8]) -> Result<Vec<String>, ModemError> {
        execute_raw_upload(
            port,
            "AT+CFTRANRX=\"C:/test.amr\",600",
            payload,
            Duration::ZERO,
            test_deadlines(),
            &mut Dispatcher::default(),
        )
    }

    #[test]
    fn com_ports_are_sorted_numerically() {
        let mut names = ["COM10", "COM2", "COM1"];
        names.sort_by_key(|name| natural_port_key(name));
        assert_eq!(names, ["COM1", "COM2", "COM10"]);
    }

    #[test]
    fn dedicated_at_interface_is_preferred() {
        let candidate = |name: &str, product: &str| PortCandidate {
            name: name.into(),
            vid: 0x1e0e,
            pid: 0x9011,
            serial_number: None,
            product: Some(product.into()),
        };
        let mut ports = [
            candidate("COM5", "SimTech HS-USB Modem 9011"),
            candidate("COM6", "SimTech HS-USB AT Port 9011 (COM6)"),
        ];
        ports.sort_by_key(candidate_sort_key);
        assert_eq!(ports[0].name, "COM6");
    }

    #[test]
    fn initialization_sequence_contains_required_modem_modes() {
        assert_eq!(
            INITIALIZATION_COMMANDS,
            ["AT+CMEE=2", "AT+CVHU=0", "AT+CMGF=1"]
        );
        assert_eq!(
            OPTIONAL_INITIALIZATION_COMMANDS,
            [
                "AT+CLCC=1",
                "AT+CNMI=2,1,0,1,0",
                "AT+CSMP=49,167,0,0",
                "AT+CSDH=1"
            ]
        );
    }

    #[test]
    fn duplicate_hardware_states_are_not_published() {
        let mut previous = None;
        let mut reports = Vec::new();
        publish_if_changed(&mut previous, HardwareState::Disconnected, &mut |state| {
            reports.push(state)
        });
        publish_if_changed(&mut previous, HardwareState::Disconnected, &mut |state| {
            reports.push(state)
        });
        assert_eq!(reports, [HardwareState::Disconnected]);
    }

    #[test]
    fn batch_restores_mode_after_success_rejection_and_timeout() {
        for failure in [
            None,
            Some(ModemError::CommandRejected("no".into())),
            Some(ModemError::Timeout),
        ] {
            let mut seen = Vec::new();
            let expect_error = failure.is_some();
            let mut failure = failure;
            let result = run_batch(
                &["AT+CMGF=0".into(), "AT+CMGL=4".into()],
                Some("AT+CMGF=1"),
                |command| {
                    seen.push(command.to_owned());
                    if command == "AT+CMGL=4" {
                        if let Some(error) = failure.take() {
                            return Err(error);
                        }
                    }
                    Ok(vec![command.into()])
                },
            );
            assert_eq!(seen.last().unwrap(), "AT+CMGF=1");
            assert_eq!(result.is_err(), expect_error);
        }
    }

    #[test]
    fn batch_finalizer_preserves_last_command_response() {
        let result = run_batch(
            &["AT+CMGF=0".into(), "AT+CMGL=4".into()],
            Some("AT+CMGF=1"),
            |command| {
                Ok(if command == "AT+CMGL=4" {
                    vec!["+CMGL: 1,1,,23".into(), "001122".into()]
                } else {
                    Vec::new()
                })
            },
        )
        .unwrap();
        assert_eq!(result, vec!["+CMGL: 1,1,,23", "001122"]);
    }

    #[test]
    fn raw_transfer_is_paced_in_256_byte_chunks_without_ctrl_z() {
        let payload = vec![0x55; 600];
        let mut writes = Vec::new();
        transfer_raw_payload(&payload, Duration::ZERO, |chunk| {
            writes.push(chunk.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            writes.iter().map(Vec::len).collect::<Vec<_>>(),
            [256, 256, 88]
        );
        assert_eq!(writes.concat(), payload);
        assert_ne!(writes.last().and_then(|chunk| chunk.last()), Some(&0x1a));
    }

    #[test]
    fn raw_upload_accepts_prompt_split_across_reads_and_final_ok() {
        let payload = vec![0x55; 600];
        let mut port = ScriptedPort::new([
            ReadStep::Bytes(b"\r\n".to_vec()),
            ReadStep::Bytes(b">".to_vec()),
            ReadStep::Bytes(b" \r\nO".to_vec()),
            ReadStep::Bytes(b"K\r\n".to_vec()),
        ]);
        raw_command(&mut port, &payload).unwrap();
        assert_eq!(port.writes[0], b"AT+CFTRANRX=\"C:/test.amr\",600");
        assert_eq!(port.writes[1], b"\r");
        assert_eq!(
            port.writes[2..].iter().map(Vec::len).collect::<Vec<_>>(),
            [256, 256, 88]
        );
        assert_eq!(port.writes[2..].concat(), payload);
        assert!(!port.writes[2..].iter().any(|write| write == &[0x1a]));
    }

    #[test]
    fn raw_upload_reports_immediate_error_without_sending_payload() {
        let payload = vec![0x55; 600];
        let mut port = ScriptedPort::new([ReadStep::Bytes(b"\r\nERROR\r\n".to_vec())]);
        assert_eq!(
            raw_command(&mut port, &payload),
            Err(ModemError::CommandRejected("ERROR".into()))
        );
        assert_eq!(port.writes.len(), 2);
    }

    #[test]
    fn raw_upload_distinguishes_missing_prompt_from_missing_final_ok() {
        let payload = vec![0x55; 600];
        let mut missing_prompt = ScriptedPort::new([ReadStep::Timeout]);
        assert!(matches!(
            raw_command(&mut missing_prompt, &payload),
            Err(ModemError::RawUploadTimeout {
                phase: RawUploadTimeoutPhase::Prompt,
                bytes_sent: 0,
                resynchronized: true,
                ..
            })
        ));
        assert_eq!(missing_prompt.writes.len(), 2);

        let mut missing_result =
            ScriptedPort::new([ReadStep::Bytes(b"\r\n> ".to_vec()), ReadStep::Timeout]);
        assert!(matches!(
            raw_command(&mut missing_result, &payload),
            Err(ModemError::RawUploadTimeout {
                phase: RawUploadTimeoutPhase::FinalResult,
                bytes_sent: 600,
                chunks_sent: 3,
                resynchronized: true,
                ..
            })
        ));
        assert_eq!(missing_result.writes[2..].concat(), payload);
    }

    #[test]
    fn timed_out_interactive_transfer_resets_framing_before_next_command() {
        let payload = vec![0x55; 10];
        let mut port = ScriptedPort::new([
            ReadStep::Bytes(b"\r\n> ".to_vec()),
            ReadStep::Bytes(b"\r\nSTALE".to_vec()),
            ReadStep::Timeout,
        ]);
        let mut dispatcher = Dispatcher::default();
        assert!(matches!(
            execute_raw_upload(
                &mut port,
                "AT+CFTRANRX=\"C:/test.amr\",10",
                &payload,
                Duration::ZERO,
                test_deadlines(),
                &mut dispatcher,
            ),
            Err(ModemError::RawUploadTimeout { .. })
        ));
        assert!(port.clears.load(Ordering::Relaxed) >= 2);
        port.reads
            .push_back(ReadStep::Bytes(b"\r\nOK\r\n".to_vec()));
        assert!(
            execute_command(
                &mut port,
                "AT",
                None,
                false,
                Duration::from_millis(8),
                PayloadMode::Sms,
                &mut dispatcher,
            )
            .unwrap()
            .is_empty()
        );
    }
}
