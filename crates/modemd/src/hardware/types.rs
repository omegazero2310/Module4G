use crate::ModemError;
use serialport::SerialPort;
use std::{io, time::Duration};

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
    pub(super) port: Box<dyn SerialPort>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HardwareState {
    Disconnected,
    PortBusy { port_name: String },
    Ready { port_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmsUrcEvent {
    DirectReport(Vec<String>),
    StoredIndication(String),
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

    pub(super) fn is_present(&self) -> bool {
        self.port.bytes_to_read().is_ok()
            && serialport::available_ports().is_ok_and(|ports| {
                ports
                    .iter()
                    .any(|port| port.port_name.eq_ignore_ascii_case(&self.port_name))
            })
    }
}
