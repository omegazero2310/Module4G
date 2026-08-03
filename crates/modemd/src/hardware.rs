use crate::{
    ModemError,
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
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const INITIALIZATION_COMMANDS: &[&str] = &["AT+CMEE=2", "AT+CVHU=0", "AT+CMGF=1"];
const OPTIONAL_INITIALIZATION_COMMANDS: &[&str] = &["AT+CLCC=1", "AT+CNMI=2,1,0,1,0", "AT+CSDH=1"];

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
    pub reply: tokio::sync::oneshot::Sender<Result<Vec<String>, ModemError>>,
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
                    let result = execute_command(
                        modem.as_mut().expect("presence checked").port(),
                        &request.command,
                        request.payload.as_deref(),
                        request.guarded,
                        if request.payload.is_some() {
                            SMS_SUBMIT_TIMEOUT
                        } else {
                            COMMAND_TIMEOUT
                        },
                        &mut dispatcher,
                    );
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

fn execute_command(
    port: &mut dyn SerialPort,
    command: &str,
    payload: Option<&[u8]>,
    guarded: bool,
    timeout: Duration,
    dispatcher: &mut Dispatcher,
) -> Result<Vec<String>, ModemError> {
    if guarded {
        crate::at::validate_console(command, false)?;
    }
    if command.starts_with("ATD") {
        dispatcher.clear_urcs();
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
                        port.write_all(payload)
                            .map_err(|_| ModemError::Disconnected)?;
                        port.write_all(&[0x1a])
                            .map_err(|_| ModemError::Disconnected)?;
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
            ["AT+CLCC=1", "AT+CNMI=2,1,0,1,0", "AT+CSDH=1"]
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
}
