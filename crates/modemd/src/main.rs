#[cfg(windows)]
mod windows_host {
    use modemd::{hardware, settings::Settings};
    use std::{
        ffi::{OsString, c_void},
        io, ptr,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
    };

    const SERVICE_NAME: &str = "A7670ModemService";
    const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";
    const SDDL_REVISION_1: u32 = 1;
    // Deny network and anonymous tokens; allow SYSTEM, administrators, LocalService,
    // and authenticated local users. PIPE_REJECT_REMOTE_CLIENTS is also enabled.
    const PIPE_SDDL: &str =
        "D:P(D;;GA;;;AN)(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)(A;;GRGW;;;AU)";

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> windows_service::Result<()> {
        if std::env::args_os().any(|arg| arg == "--scan") {
            return run_scan();
        }
        if std::env::args_os().any(|arg| arg == "--console") {
            return run_console();
        }
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
    }

    fn run_scan() -> windows_service::Result<()> {
        let settings = Settings::default();
        println!(
            "Looking for {:04X}:{:04X} at {} baud",
            settings.usb_vid, settings.usb_pid, settings.baud
        );
        match serialport::available_ports() {
            Ok(ports) if ports.is_empty() => println!("No serial ports found."),
            Ok(ports) => {
                for port in ports {
                    println!("{}: {:?}", port.port_name, port.port_type);
                }
            }
            Err(error) => println!("Enumeration failed: {error}"),
        }
        match hardware::discover_and_initialize(&settings) {
            Ok(modem) => println!("AT modem initialized successfully on {}", modem.port_name),
            Err(error) => println!("AT modem detection failed: {error}"),
        }
        Ok(())
    }

    fn run_console() -> windows_service::Result<()> {
        println!("A7670ModemService console mode on {PIPE}");
        tokio::runtime::Runtime::new()
            .map_err(windows_service::Error::Winapi)?
            .block_on(serve(async { std::future::pending::<()>().await }))
            .map_err(windows_service::Error::Winapi)
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            eprintln!("{SERVICE_NAME} failed: {error}");
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = stop_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        status_handle.set_service_status(status(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ))?;
        let runtime = tokio::runtime::Runtime::new().map_err(windows_service::Error::Winapi)?;
        let result = runtime.block_on(serve(async move {
            let _ = tokio::task::spawn_blocking(move || stop_rx.recv()).await;
        }));
        status_handle
            .set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
        result.map_err(windows_service::Error::Winapi)
    }

    fn status(state: ServiceState, accepted: ServiceControlAccept) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::ZERO,
            process_id: None,
        }
    }

    async fn serve(stop: impl Future<Output = ()>) -> io::Result<()> {
        let device_state = Arc::new(RwLock::new(hardware::HardwareState::Disconnected));
        let monitor_state = Arc::clone(&device_state);
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor_stop_task = Arc::clone(&monitor_stop);
        let (command_tx, command_rx) = mpsc::channel();
        let monitor = tokio::task::spawn_blocking(move || {
            hardware::monitor_with_commands(
                Settings::default(),
                monitor_stop_task,
                |state| {
                    match &state {
                        hardware::HardwareState::Ready { port_name } => {
                            eprintln!("modem connected and initialized on {port_name}")
                        }
                        hardware::HardwareState::PortBusy { port_name } => {
                            eprintln!("modem detected on {port_name}, but the port is busy")
                        }
                        hardware::HardwareState::Disconnected => eprintln!("modem disconnected"),
                    }
                    *monitor_state
                        .write()
                        .unwrap_or_else(|lock| lock.into_inner()) = state;
                },
                command_rx,
            );
        });
        tokio::pin!(stop);
        loop {
            let security = PipeSecurity::new()?;
            let server = unsafe {
                ServerOptions::new()
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(PIPE, security.attributes_ptr())?
            };
            tokio::select! {
                _ = &mut stop => {
                    monitor_stop.store(true, Ordering::Relaxed);
                    monitor.await.map_err(io::Error::other)?;
                    return Ok(());
                },
                connected = server.connect() => {
                    connected?;
                    tokio::spawn(handle_client(server, Arc::clone(&device_state), command_tx.clone()));
                }
            }
        }
    }

    struct PipeSecurity {
        descriptor: PSECURITY_DESCRIPTOR,
        attributes: SECURITY_ATTRIBUTES,
    }

    impl PipeSecurity {
        fn new() -> io::Result<Self> {
            let encoded: Vec<u16> = PIPE_SDDL.encode_utf16().chain(Some(0)).collect();
            let mut descriptor = ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    encoded.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            Ok(Self {
                descriptor,
                attributes,
            })
        }

        fn attributes_ptr(&self) -> *mut c_void {
            (&self.attributes as *const SECURITY_ATTRIBUTES)
                .cast_mut()
                .cast()
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            unsafe { LocalFree(self.descriptor.cast()) };
        }
    }

    async fn handle_client(
        server: tokio::net::windows::named_pipe::NamedPipeServer,
        device_state: Arc<RwLock<hardware::HardwareState>>,
        command_tx: mpsc::Sender<hardware::AtRequest>,
    ) -> io::Result<()> {
        let mut stream = BufReader::new(server);
        let mut line = String::new();
        while stream.read_line(&mut line).await? != 0 {
            let request = line.trim();
            let response = if request == "STATUS" {
                let state = device_state.read().unwrap_or_else(|lock| lock.into_inner());
                let state = match &*state {
                    hardware::HardwareState::Ready { port_name } => format!("Ready\t{port_name}"),
                    hardware::HardwareState::PortBusy { port_name } => {
                        format!("Port busy\t{port_name}")
                    }
                    hardware::HardwareState::Disconnected => "Disconnected\t".to_owned(),
                };
                format!("STATUS\t0.1.0\t{state}\tUNKNOWN\tNot registered\t0\n")
            } else if let Some(rest) = request.strip_prefix("SMS|") {
                let mut fields = rest.splitn(2, '|');
                let destination = fields.next().unwrap_or_default();
                let body = fields.next().and_then(decode_hex);
                match (modemd::sms::normalize_number(destination), body) {
                    (Ok(destination), Some(body)) => {
                        run_actor(
                            &command_tx,
                            format!("AT+CMGS=\"{destination}\""),
                            Some(body),
                            false,
                        )
                        .await
                    }
                    (Err(error), _) => format!("ERROR: {error}\n"),
                    (_, None) => "ERROR: invalid SMS payload\n".into(),
                }
            } else if request == "BALANCE" {
                check_viettel_balance(&command_tx).await
            } else if let Some(code) = request.strip_prefix("USSD|") {
                if code.is_empty() || code.len() > 64 {
                    "ERROR: invalid USSD code\n".into()
                } else {
                    run_actor(&command_tx, format!("AT+CUSD=1,\"{code}\",15"), None, false).await
                }
            } else if let Some(number) = request.strip_prefix("DIAL|") {
                match modemd::sms::normalize_number(number) {
                    Ok(number) => {
                        run_actor(&command_tx, format!("ATD{number};"), None, false).await
                    }
                    Err(error) => format!("ERROR: {error}\n"),
                }
            } else if request == "HANGUP" {
                run_actor(&command_tx, "ATH".into(), None, false).await
            } else if request == "CALLSTATUS" {
                run_actor(&command_tx, "AT+CLCC".into(), None, false).await
            } else if request.starts_with("AT") {
                match modemd::at::validate_console(request, false) {
                    Err(error) => format!("ERROR: {error}\n"),
                    Ok(command) => {
                        let (reply, response) = tokio::sync::oneshot::channel();
                        if command_tx
                            .send(hardware::AtRequest {
                                command,
                                payload: None,
                                guarded: true,
                                reply,
                            })
                            .is_err()
                        {
                            "ERROR: modem command actor unavailable\n".to_owned()
                        } else {
                            match tokio::time::timeout(Duration::from_secs(3), response).await {
                                Ok(Ok(Ok(lines))) => format!("{}\n", lines.join("\r\n")),
                                Ok(Ok(Err(error))) => format!("ERROR: {error}\n"),
                                Ok(Err(_)) => "ERROR: modem command actor stopped\n".to_owned(),
                                Err(_) => "ERROR: command timed out\n".to_owned(),
                            }
                        }
                    }
                }
            } else {
                "ERROR: unsupported request\n".to_owned()
            };
            stream.get_mut().write_all(response.as_bytes()).await?;
            stream.get_mut().flush().await?;
            line.clear();
        }
        Ok(())
    }

    async fn run_actor(
        command_tx: &mpsc::Sender<hardware::AtRequest>,
        command: String,
        payload: Option<Vec<u8>>,
        guarded: bool,
    ) -> String {
        let (reply, response) = tokio::sync::oneshot::channel();
        if command_tx
            .send(hardware::AtRequest {
                command,
                payload,
                guarded,
                reply,
            })
            .is_err()
        {
            return "ERROR: modem command actor unavailable\n".into();
        }
        match tokio::time::timeout(Duration::from_secs(35), response).await {
            Ok(Ok(Ok(lines))) => format!("{}\n", lines.join(" | ")),
            Ok(Ok(Err(error))) => format!("ERROR: {error}\n"),
            Ok(Err(_)) => "ERROR: modem command actor stopped\n".into(),
            Err(_) => "ERROR: command timed out\n".into(),
        }
    }

    async fn check_viettel_balance(command_tx: &mpsc::Sender<hardware::AtRequest>) -> String {
        let submitted = run_actor(
            command_tx,
            "AT+CMGS=\"191\"".into(),
            Some(b"TK".to_vec()),
            false,
        )
        .await;
        if submitted.starts_with("ERROR:") {
            return submitted;
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            let response =
                run_actor(command_tx, "AT+CMGL=\"REC UNREAD\"".into(), None, false).await;
            if response.starts_with("ERROR:") {
                return response;
            }
            if let Some(message) = viettel_balance_message(&response) {
                return format!("{message}\n");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        "ERROR: timed out waiting for the balance SMS from 191\n".into()
    }

    fn viettel_balance_message(response: &str) -> Option<String> {
        let parts: Vec<_> = response.split(" | ").map(str::trim).collect();
        let header = parts.iter().position(|part| {
            part.starts_with("+CMGL:")
                && csv_field(part, 2).is_some_and(|sender| {
                    sender.trim().trim_matches('\"').eq_ignore_ascii_case("191")
                })
        })?;
        let body: Vec<_> = parts[header + 1..]
            .iter()
            .take_while(|part| !part.starts_with("+CMGL:") && **part != "OK")
            .copied()
            .collect();
        if body.is_empty() {
            return None;
        }

        let body = body.join("\n");
        let dcs = csv_field(parts[header], 8).and_then(|value| value.trim().parse::<u8>().ok());
        let is_ucs2 = dcs.is_some_and(dcs_uses_ucs2)
            || (dcs.is_none() && body.starts_with("00") && body.len() % 4 == 0);
        if is_ucs2 {
            decode_ucs2_hex(&body)
        } else {
            Some(body)
        }
    }

    fn dcs_uses_ucs2(dcs: u8) -> bool {
        (dcs & 0xc0 == 0 && dcs & 0x0c == 0x08) || dcs & 0xf0 == 0xe0
    }

    fn csv_field(value: &str, wanted: usize) -> Option<&str> {
        let mut quoted = false;
        let mut field = 0;
        let mut start = 0;
        for (index, byte) in value.bytes().enumerate() {
            match byte {
                b'"' => quoted = !quoted,
                b',' if !quoted => {
                    if field == wanted {
                        return Some(value[start..index].trim());
                    }
                    field += 1;
                    start = index + 1;
                }
                _ => {}
            }
        }
        (field == wanted).then(|| value[start..].trim())
    }

    fn decode_ucs2_hex(value: &str) -> Option<String> {
        if value.is_empty()
            || value.len() % 4 != 0
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let units = (0..value.len())
            .step_by(4)
            .map(|index| u16::from_str_radix(&value[index..index + 4], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .ok()
    }

    fn decode_hex(value: &str) -> Option<Vec<u8>> {
        if value.len() % 2 != 0 {
            return None;
        }
        (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::viettel_balance_message;

        #[test]
        fn extracts_viettel_balance_sms_from_191() {
            let response = "+CMGL: 7,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\" | Thuê bao: 84XXXXXXXXX (HISCL): | - TK gốc: 85.500đ, HSD: 00:00:00 02-10-2026. | - TK tiền di động: 863đ, HSD: 00:00:00 01-01-2100. | - TK tiền khuyến mại: 89.174đ. | OK";
            let message = viettel_balance_message(response).unwrap();
            assert!(message.contains("TK gốc: 85.500đ"));
            assert!(message.contains("TK tiền khuyến mại: 89.174đ"));
        }

        #[test]
        fn ignores_messages_from_other_senders() {
            assert!(
                viettel_balance_message(
                    "+CMGL: 8,\"REC UNREAD\",\"123\",\"\",\"\" | Not a balance | OK"
                )
                .is_none()
            );
        }

        #[test]
        fn decodes_ucs2_viettel_balance_sms_using_dcs() {
            let response = "+CMGL: 9,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,8,\"+84000000000\",145,67 | 00540068007500EA002000620061006F003A002000380034005800580058005800580058005800580058002000280054004F004D003600390030005F003100320029003A0020000A002D00200054004B002000671ED10063003A002000310039002E0038003000300111002C0020004800530044003A002000300030003A00300030003A0030 | OK";
            assert_eq!(
                viettel_balance_message(response).unwrap(),
                "Thuê bao: 84XXXXXXXXX (TOM690_12): \n- TK gốc: 19.800đ, HSD: 00:00:0"
            );
        }

        #[test]
        fn does_not_decode_gsm_7_bit_hex_looking_text() {
            let response = "+CMGL: 10,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,0,\"+84000000000\",145,4 | 1234 | OK";
            assert_eq!(viettel_balance_message(response).unwrap(), "1234");
        }
    }
}

#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    windows_host::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("A7670ModemService is supported only on Windows.");
}
