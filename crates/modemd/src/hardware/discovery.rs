use super::*;

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
            .dtr_on_open(true)
            .open();
        let mut port = match opened {
            Ok(port) => port,
            Err(source)
                if is_dedicated_at_port(&candidate)
                    || settings
                        .port_override
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&candidate.name)) =>
            {
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
        if let Err(error) = send_expect_ok(port, command, COMMAND_TIMEOUT) {
            eprintln!("optional modem initialization failed for {command}: {error}");
        }
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

pub(super) fn natural_port_key(name: &str) -> (String, u32) {
    let split = name
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(name.len());
    let (prefix, suffix) = name.split_at(split);
    (
        prefix.to_ascii_uppercase(),
        suffix.parse().unwrap_or(u32::MAX),
    )
}

pub(super) fn candidate_sort_key(candidate: &PortCandidate) -> (bool, (String, u32)) {
    (
        !is_dedicated_at_port(candidate),
        natural_port_key(&candidate.name),
    )
}

pub(super) fn is_dedicated_at_port(candidate: &PortCandidate) -> bool {
    candidate
        .product
        .as_deref()
        .is_some_and(|product| product.to_ascii_lowercase().contains("at port"))
}
