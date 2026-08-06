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
const SMS_STORAGE_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const SMS_PROMPT_TIMEOUT: Duration = Duration::from_secs(2);
const SMS_SUBMIT_TIMEOUT: Duration = Duration::from_secs(40);
const RAW_PROMPT_TIMEOUT: Duration = Duration::from_secs(2);
const RAW_RESULT_TIMEOUT: Duration = Duration::from_secs(30);
const INTERACTIVE_RESYNC_WINDOW: Duration = Duration::from_millis(300);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const INITIALIZATION_COMMANDS: &[&str] = &["AT+CMEE=2", "AT+CVHU=0", "AT+CMGF=1"];
const OPTIONAL_INITIALIZATION_COMMANDS: &[&str] = &[
    "AT+CLCC=1",
    "AT+CPMS=\"SM\",\"SM\",\"SM\"",
    "AT+CPMS?",
    "AT+CSDH=1",
];

mod command;
mod discovery;
mod interactive;
mod types;
use command::*;
#[cfg(test)]
use discovery::{candidate_sort_key, natural_port_key};
pub use discovery::{discover_and_initialize, enumerate};
use interactive::*;
pub use types::{
    AtRequest, HardwareError, HardwareState, InitializedModem, PayloadMode, PortCandidate,
    SmsUrcEvent,
};

/// Continuously discovers the modem and reports plug/unplug state changes.
///
/// The returned serial handle stays owned by this loop until removal, ensuring
/// no second process can claim the AT port between initialization and use.
pub fn monitor(settings: Settings, stop: Arc<AtomicBool>, mut report: impl FnMut(HardwareState)) {
    let (_sender, receiver) = mpsc::channel();
    let (sms_sender, _sms_receiver) = mpsc::channel();
    monitor_with_commands(settings, stop, &mut report, receiver, sms_sender);
}

/// Owns the serial port and executes guarded console requests sequentially.
pub fn monitor_with_commands(
    settings: Settings,
    stop: Arc<AtomicBool>,
    mut report: impl FnMut(HardwareState),
    commands: mpsc::Receiver<AtRequest>,
    sms_events: mpsc::Sender<SmsUrcEvent>,
) {
    let mut modem: Option<InitializedModem> = None;
    let mut dispatcher = Dispatcher::default();
    let mut last_state: Option<HardwareState> = None;
    let mut next_health_check = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if modem.as_ref().is_some_and(InitializedModem::is_present) {
            let mut connection_failed = false;
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(request) => {
                    let port = modem.as_mut().expect("presence checked").port();
                    let result = if request.batch.is_empty() {
                        execute_command(
                            port,
                            &request.command,
                            request.payload.as_deref(),
                            request.guarded,
                            request.payload.as_ref().map_or_else(
                                || command_timeout(&request.command),
                                |_| {
                                    payload_timeout(
                                        request.payload.as_deref(),
                                        request.payload_mode,
                                    )
                                },
                            ),
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
                                command_timeout(command),
                                PayloadMode::Sms,
                                &mut dispatcher,
                            )
                        })
                    };
                    publish_sms_events(&mut dispatcher, &sms_events);
                    if result_confirms_liveness(&result) {
                        next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
                    }
                    connection_failed = result_requires_reconnect(&result);
                    let _ = request.reply.send(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(DEVICE_POLL_INTERVAL),
            }
            if connection_failed {
                eprintln!("modem command did not respond; closing the COM session for recovery");
                modem = None;
                dispatcher.reset();
                publish_if_changed(&mut last_state, HardwareState::Disconnected, &mut report);
                continue;
            }
            // The serial actor remains the sole reader even while idle. This
            // prevents unsolicited delivery reports from filling the driver
            // buffer or being mistaken for a later command response.
            if let Some(connected) = modem.as_mut() {
                let mut buffer = [0_u8; 256];
                match connected.port().read(&mut buffer) {
                    Ok(count) if count > 0 => {
                        let _ = dispatcher.push(&buffer[..count], None);
                        publish_sms_events(&mut dispatcher, &sms_events);
                        next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                    Err(_) => modem = None,
                }
            }
            if modem.is_none() {
                dispatcher.reset();
                publish_if_changed(&mut last_state, HardwareState::Disconnected, &mut report);
                continue;
            }
            if modem.is_some() && Instant::now() >= next_health_check {
                let result = execute_command(
                    modem.as_mut().expect("presence checked").port(),
                    "AT",
                    None,
                    false,
                    PROBE_TIMEOUT,
                    PayloadMode::Sms,
                    &mut dispatcher,
                );
                publish_sms_events(&mut dispatcher, &sms_events);
                if result_requires_reconnect(&result) {
                    eprintln!(
                        "modem health probe did not respond; closing the COM session for recovery"
                    );
                    modem = None;
                    dispatcher.reset();
                    publish_if_changed(&mut last_state, HardwareState::Disconnected, &mut report);
                } else {
                    next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
                }
            }
            continue;
        }

        if modem.take().is_some() {
            eprintln!("modem COM device became unavailable; reopening the serial session");
            publish_if_changed(&mut last_state, HardwareState::Disconnected, &mut report);
        }
        dispatcher.reset();
        match discover_and_initialize(&settings) {
            Ok(connected) => {
                let state = HardwareState::Ready {
                    port_name: connected.port_name.clone(),
                };
                modem = Some(connected);
                next_health_check = Instant::now() + HEALTH_CHECK_INTERVAL;
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
                "AT+CPMS=\"SM\",\"SM\",\"SM\"",
                "AT+CPMS?",
                "AT+CSDH=1"
            ]
        );
    }

    #[test]
    fn sms_configuration_and_storage_commands_allow_the_documented_response_window() {
        for command in [
            "AT+CNMI=2,1,0,1,0",
            "AT+CSMP?",
            "AT+CPMS?",
            "AT+CMGL=4",
            "AT+CMGD=40",
        ] {
            assert_eq!(command_timeout(command), SMS_STORAGE_COMMAND_TIMEOUT);
        }
        assert_eq!(command_timeout("AT+CSQ"), COMMAND_TIMEOUT);
    }

    #[test]
    fn response_timeouts_force_a_fresh_serial_session() {
        for error in [
            ModemError::Disconnected,
            ModemError::Timeout,
            ModemError::SmsSubmitTimeout {
                phase: "test",
                resynchronized: true,
            },
            ModemError::RawUploadTimeout {
                phase: RawUploadTimeoutPhase::Prompt,
                bytes_sent: 0,
                chunks_sent: 0,
                pacing_ms: 0,
                elapsed_ms: 1,
                resynchronized: true,
            },
        ] {
            assert!(result_requires_reconnect(&Err(error)));
        }
        assert!(!result_requires_reconnect(&Err(
            ModemError::CommandRejected("ERROR".into())
        )));
        assert!(result_confirms_liveness(&Err(ModemError::CommandRejected(
            "ERROR".into()
        ))));
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
    fn sms_submission_completes_on_cmgs_and_ok_with_interleaved_text_report() {
        let mut port = ScriptedPort::new([
            ReadStep::Bytes(b"\r\n> ".to_vec()),
            ReadStep::Bytes(b"+CDS: 2,7,\"+66812345678\",145,\"26/08/04,12:00:00+00\",\"26/08/04,12:01:00+00\",0\r\n+CMGS: 8\r\nOK\r\n".to_vec()),
        ]);
        let mut dispatcher = Dispatcher::default();
        let lines = execute_sms_submit_with_deadlines(
            &mut port,
            "AT+CMGS=\"+66812345678\"",
            b"hello",
            &mut dispatcher,
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(lines, vec!["+CMGS: 8"]);
        assert_eq!(port.writes[2], b"hello");
        assert_eq!(port.writes[3], [0x1a]);
        assert_eq!(dispatcher.take_complete_sms_urcs().len(), 1);
    }

    #[test]
    fn sms_result_timeout_resynchronizes_before_next_command() {
        let mut port = ScriptedPort::new([
            ReadStep::Bytes(b"\r\n> ".to_vec()),
            ReadStep::Timeout,
            ReadStep::Bytes(b"+CMGS: 8\r\nOK\r\n".to_vec()),
        ]);
        let error = execute_sms_submit_with_deadlines(
            &mut port,
            "AT+CMGS=\"+66812345678\"",
            b"hello",
            &mut Dispatcher::default(),
            Duration::from_millis(5),
            Duration::from_millis(1),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ModemError::SmsSubmitTimeout {
                phase: "after payload transmission",
                ..
            }
        ));
        assert!(port.clears.load(Ordering::Relaxed) >= 2);
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
