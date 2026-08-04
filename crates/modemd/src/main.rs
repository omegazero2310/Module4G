#[cfg(windows)]
mod windows_host {
    use modemd::{
        call_workflow::CallManager,
        hardware::{self, PayloadMode},
        settings::Settings,
        storage::{BalanceRecord, SmsRecord, Store},
    };
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
        let data_dir = std::env::var_os("ProgramData")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("A7670 Modem");
        std::fs::create_dir_all(&data_dir)?;
        let store =
            Arc::new(Store::open(data_dir.join("modemd.sqlite3")).map_err(io::Error::other)?);
        let _ = store
            .recover_interrupted_calls(now())
            .map_err(io::Error::other)?;
        let _ = store.recover_interrupted_sms().map_err(io::Error::other)?;
        let settings = Arc::new(RwLock::new(
            store.load_settings().map_err(io::Error::other)?,
        ));
        let device_state = Arc::new(RwLock::new(hardware::HardwareState::Disconnected));
        let monitor_state = Arc::clone(&device_state);
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor_stop_task = Arc::clone(&monitor_stop);
        let monitor_settings = Arc::clone(&settings);
        let (command_tx, command_rx) = mpsc::channel();
        let monitor = tokio::task::spawn_blocking(move || {
            hardware::monitor_with_commands(
                monitor_settings
                    .read()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .clone(),
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
        let call_manager = Arc::new(CallManager::new(
            command_tx.clone(),
            Arc::clone(&store),
            Arc::clone(&settings),
        ));
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
                    tokio::spawn(handle_client(server, Arc::clone(&device_state), command_tx.clone(), Arc::clone(&store), Arc::clone(&call_manager)));
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
        store: Arc<Store>,
        call_manager: Arc<CallManager>,
    ) -> io::Result<()> {
        let mut stream = BufReader::new(server);
        let mut line = String::new();
        while stream.read_line(&mut line).await? != 0 {
            let request = line.trim();
            let response = if request.starts_with('{') {
                handle_json(request, &command_tx, &store, &call_manager).await
            } else if request == "STATUS" {
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
                match check_viettel_balance(&command_tx, &store).await {
                    Ok((body, _)) => format!("{body}\n"),
                    Err(error) => format!("ERROR: {error}\n"),
                }
            } else if let Some(code) = request.strip_prefix("USSD|") {
                if code.is_empty() || code.len() > 64 {
                    "ERROR: invalid USSD code\n".into()
                } else {
                    run_actor(&command_tx, format!("AT+CUSD=1,\"{code}\",15"), None, false).await
                }
            } else if let Some(number) = request.strip_prefix("DIAL|") {
                match modemd::sms::normalize_number(number) {
                    Ok(number) => dial_with_release_retry(&command_tx, &number).await,
                    Err(error) => format!("ERROR: {error}\n"),
                }
            } else if request == "HANGUP" {
                run_actor(&command_tx, "ATH".into(), None, false).await
            } else if request == "CALLSTATUS" {
                run_actor(&command_tx, "AT+CLCC".into(), None, false).await
            } else if request == "CALLCAUSE" {
                run_actor(&command_tx, "AT+CEER".into(), None, false).await
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
                                payload_mode: PayloadMode::Sms,
                                batch: Vec::new(),
                                finalizer: None,
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

    async fn handle_json(
        request: &str,
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
        call_manager: &Arc<CallManager>,
    ) -> String {
        let value: serde_json::Value = match serde_json::from_str(request) {
            Ok(v) => v,
            Err(e) => return json_error(e),
        };
        let command = value
            .get("command")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        let result: Result<serde_json::Value, String> = match command {
            "list_sms" => store
                .list_sms(1000)
                .map(|mut records| {
                    // Normalize rows synchronized by older versions too, so an
                    // upgrade fixes the visible list without deleting history.
                    for record in &mut records {
                        let explicit = record.encoding.eq_ignore_ascii_case("UCS2")
                            || (record.dcs >= 0
                                && ((record.dcs as u8 & 0xc0 == 0
                                    && record.dcs as u8 & 0x0c == 0x08)
                                    || record.dcs as u8 & 0xf0 == 0xe0));
                        if let Some(body) = modemd::sms::decode_ucs2_body(&record.body, explicit) {
                            record.body = body;
                            record.encoding = "UCS2".into();
                            record.length = record.body.chars().count() as i32;
                        }
                    }
                    serde_json::to_value(records).unwrap()
                })
                .map_err(|e| e.to_string()),
            "list_balances" => store
                .list_balances(1000)
                .map(|x| serde_json::to_value(x).unwrap())
                .map_err(|e| e.to_string()),
            "send_sms" => send_sms_json(value, tx, store)
                .await
                .map(|x| serde_json::to_value(x).unwrap()),
            "sync_sms" => sync_sms_json(tx, store)
                .await
                .map(|x| serde_json::json!({"count":x})),
            "check_balance" => balance_json(tx, store)
                .await
                .map(|x| serde_json::to_value(x).unwrap()),
            "get_current_audio" => call_manager
                .current_audio()
                .map(|audio| serde_json::to_value(audio).unwrap())
                .map_err(|e| e.to_string()),
            "list_audio" => call_manager
                .list_audio()
                .map(|audio| serde_json::to_value(audio).unwrap())
                .map_err(|e| e.to_string()),
            "get_call_data" => call_manager
                .list_calls(1000)
                .and_then(|calls| {
                    call_manager
                        .list_audio()
                        .map(|audio| serde_json::json!({"calls": calls, "audio": audio}))
                })
                .map_err(|e| e.to_string()),
            "select_audio" => call_manager
                .select_audio(
                    value
                        .get("audioId")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default(),
                )
                .map(|audio| serde_json::to_value(audio).unwrap())
                .map_err(|e| e.to_string()),
            "upload_audio" => upload_audio_json(value, call_manager)
                .await
                .map(|audio| serde_json::to_value(audio).unwrap()),
            "make_call" => make_call_json(value, call_manager)
                .await
                .map(|call| serde_json::to_value(call).unwrap()),
            "hang_up" => call_manager
                .hang_up()
                .await
                .map(|()| serde_json::Value::Null)
                .map_err(|e| e.to_string()),
            "list_calls" => call_manager
                .list_calls(1000)
                .map(|calls| serde_json::to_value(calls).unwrap())
                .map_err(|e| e.to_string()),
            "get_settings" => Ok(serde_json::to_value(call_manager.settings()).unwrap()),
            "update_settings" => serde_json::from_value::<Settings>(
                value
                    .get("settings")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|_| "invalid settings payload".to_owned())
            .and_then(|settings| {
                call_manager
                    .update_settings(settings)
                    .map_err(|error| error.to_string())
            })
            .map(|settings| serde_json::to_value(settings).unwrap()),
            _ => Err("unknown JSON command".into()),
        };
        match result {
            Ok(data) => serde_json::json!({"ok":true,"data":data}).to_string() + "\n",
            Err(error) => serde_json::json!({"ok":false,"error":error}).to_string() + "\n",
        }
    }
    fn json_error(e: impl std::fmt::Display) -> String {
        serde_json::json!({"ok":false,"error":e.to_string()}).to_string() + "\n"
    }
    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    async fn upload_audio_json(
        value: serde_json::Value,
        manager: &CallManager,
    ) -> Result<modemd::storage::UploadedAudioRecord, String> {
        let name = value
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        let data: Vec<u8> = serde_json::from_value(
            value
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|_| "invalid audio payload".to_owned())?;
        manager
            .upload_audio(name, data)
            .await
            .map_err(|e| e.to_string())
    }

    async fn make_call_json(
        value: serde_json::Value,
        manager: &Arc<CallManager>,
    ) -> Result<modemd::storage::CallRecord, String> {
        let destination = modemd::sms::normalize_number(
            value
                .get("destination")
                .and_then(|value| value.as_str())
                .unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        let audio_id = value
            .get("audioId")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        manager
            .make_call(destination, audio_id)
            .await
            .map_err(|e| e.to_string())
    }
    async fn send_sms_json(
        v: serde_json::Value,
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
    ) -> Result<SmsRecord, String> {
        let peer = modemd::sms::normalize_number(
            v.get("destination")
                .and_then(|x| x.as_str())
                .unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        let body = v
            .get("body")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        modemd::sms::validate_body(&body).map_err(|e| e.to_string())?;
        let mut record = SmsRecord {
            id: ulid::Ulid::new().to_string(),
            direction: "outbound".into(),
            peer: peer.clone(),
            body: body.clone(),
            state: "sending".into(),
            created_at_ms: now(),
            kind: "submitted".into(),
            source: "app".into(),
            part_count: 1,
            parts_received: 1,
            multipart_complete: true,
            encoding: modemd::sms::validate_body(&body).unwrap_or("").into(),
            length: body.chars().count() as i32,
            storage_index: -1,
            ..Default::default()
        };
        store.save_sms(&record).map_err(|e| e.to_string())?;
        let result = actor_lines(
            tx,
            format!("AT+CMGS=\"{peer}\""),
            Some(body.clone().into_bytes()),
        )
        .await;
        let lines = match result {
            Ok(lines) => lines,
            Err(error) => {
                record.state = if is_explicit_send_rejection(&error) {
                    "send-failed"
                } else {
                    "send-unknown"
                }
                .into();
                record.cause = error.clone();
                store.save_sms(&record).map_err(|e| e.to_string())?;
                return Err(error);
            }
        };
        let mr = lines
            .iter()
            .find_map(|x| x.strip_prefix("+CMGS:").map(|v| v.trim().to_owned()))
            .unwrap_or_default();
        if mr.is_empty() {
            record.state = "send-unknown".into();
            record.cause = "modem returned OK without a +CMGS message reference".into();
            store.save_sms(&record).map_err(|e| e.to_string())?;
            return Err(record.cause.clone());
        }
        record.state = "submitted".into();
        record.message_reference = mr;
        record.cause.clear();
        store.save_sms(&record).map_err(|e| e.to_string())?;
        Ok(record)
    }

    fn is_explicit_send_rejection(error: &str) -> bool {
        let upper = error.to_ascii_uppercase();
        upper.contains("COMMAND REJECTED")
            || upper.contains("+CMS ERROR")
            || upper.contains("+CME ERROR")
            || upper.trim_end().ends_with("ERROR")
    }
    async fn actor_lines(
        tx: &mpsc::Sender<hardware::AtRequest>,
        command: String,
        payload: Option<Vec<u8>>,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(hardware::AtRequest {
            command,
            payload,
            guarded: false,
            payload_mode: PayloadMode::Sms,
            batch: Vec::new(),
            finalizer: None,
            reply,
        })
        .map_err(|_| "modem command actor unavailable".to_owned())?;
        tokio::time::timeout(Duration::from_secs(35), rx)
            .await
            .map_err(|_| "modem command timed out".to_owned())?
            .map_err(|_| "modem command actor stopped".to_owned())?
            .map_err(|e| e.to_string())
    }
    async fn sync_sms_json(
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
    ) -> Result<usize, String> {
        let lines = actor_pdu_snapshot(tx).await?;
        let stamp = now();
        let records = snapshot_records(modemd::sms::parse_cmgl(&lines), stamp);
        store.sync_sms(&records, stamp).map_err(|e| e.to_string())?;
        Ok(records.len())
    }

    fn snapshot_records(parsed: Vec<modemd::sms::SimSms>, stamp: i64) -> Vec<SmsRecord> {
        parsed
            .into_iter()
            .map(|x| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                let single_sim_identity = (x.part_count == 1).then_some(x.index);
                let immutable_body = (x.part_count == 1).then_some(x.body.as_str());
                (
                    single_sim_identity,
                    &x.direction,
                    &x.peer,
                    immutable_body,
                    &x.modem_timestamp,
                    &x.kind,
                    &x.message_reference,
                    &x.delivery_status,
                )
                    .hash(&mut h);
                let created_at_ms = modem_timestamp_ms(&x.modem_timestamp).unwrap_or(stamp);
                SmsRecord {
                    id: ulid::Ulid::new().to_string(),
                    direction: x.direction,
                    peer: x.peer,
                    body: x.body,
                    state: match x.modem_status.as_str() {
                        "REC UNREAD" => "unread",
                        "REC READ" => "read",
                        "STO SENT" => "submitted",
                        "STO UNSENT" => "send-failed",
                        _ => "status-report",
                    }
                    .into(),
                    message_reference: x.message_reference,
                    created_at_ms,
                    kind: x.kind,
                    source: "sim".into(),
                    storage: "SM".into(),
                    storage_index: x.index,
                    storage_indices: x.storage_indices,
                    part_count: x.part_count,
                    parts_received: x.parts_received,
                    multipart_complete: x.multipart_complete,
                    part_payloads: x.part_payloads,
                    part_timestamps: x.part_timestamps,
                    modem_status: x.modem_status,
                    modem_timestamp: x.modem_timestamp,
                    encoding: x.encoding,
                    dcs: x.dcs,
                    length: x.length,
                    service_center: x.service_center,
                    delivery_status: x.delivery_status,
                    synchronized_at_ms: stamp,
                    present_on_modem: true,
                    fingerprint: format!("{:016x}", h.finish()),
                    ..Default::default()
                }
            })
            .collect()
    }

    fn modem_timestamp_ms(value: &str) -> Option<i64> {
        let bytes = value.as_bytes();
        if bytes.len() != 20
            || bytes[2] != b'/'
            || bytes[5] != b'/'
            || bytes[8] != b','
            || bytes[11] != b':'
            || bytes[14] != b':'
            || !matches!(bytes[17], b'+' | b'-')
        {
            return None;
        }
        let number = |start: usize| -> Option<i64> {
            std::str::from_utf8(&bytes[start..start + 2])
                .ok()?
                .parse()
                .ok()
        };
        let (year, month, day) = (2000 + number(0)?, number(3)?, number(6)?);
        let (hour, minute, second) = (number(9)?, number(12)?, number(15)?);
        let quarters = number(18)?;
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let max_day = match month {
            2 => {
                if leap {
                    29
                } else {
                    28
                }
            }
            4 | 6 | 9 | 11 => 30,
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            _ => return None,
        };
        if !(1..=12).contains(&month)
            || !(1..=max_day).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
            || quarters > 79
        {
            return None;
        }
        let adjusted_year = year - i64::from(month <= 2);
        let era = adjusted_year.div_euclid(400);
        let yoe = adjusted_year - era * 400;
        let shifted_month = month + if month > 2 { -3 } else { 9 };
        let doy = (153 * shifted_month + 2) / 5 + day - 1;
        let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100) + doy - 719468;
        let local_seconds = days * 86400 + hour * 3600 + minute * 60 + second;
        let offset = quarters * 15 * 60 * if bytes[17] == b'-' { -1 } else { 1 };
        Some((local_seconds - offset) * 1000)
    }
    async fn actor_pdu_snapshot(
        tx: &mpsc::Sender<hardware::AtRequest>,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(hardware::AtRequest {
            command: String::new(),
            payload: None,
            guarded: false,
            payload_mode: PayloadMode::Sms,
            batch: vec![
                "AT+CPMS=\"SM\",\"SM\",\"SM\"".into(),
                "AT+CMGF=0".into(),
                "AT+CMGL=4".into(),
            ],
            finalizer: Some("AT+CMGF=1".into()),
            reply,
        })
        .map_err(|_| "modem command actor unavailable".to_owned())?;
        tokio::time::timeout(Duration::from_secs(35), rx)
            .await
            .map_err(|_| "modem command timed out".to_owned())?
            .map_err(|_| "modem command actor stopped".to_owned())?
            .map_err(|e| e.to_string())
    }
    async fn balance_json(
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
    ) -> Result<BalanceRecord, String> {
        let (raw, sms_id) = check_viettel_balance(tx, store).await?;
        let stamp = now();
        let b = BalanceRecord {
            id: ulid::Ulid::new().to_string(),
            raw,
            created_at_ms: stamp,
            sms_id,
            ..Default::default()
        };
        store.save_balance(&b).map_err(|e| e.to_string())?;
        Ok(b)
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
                payload_mode: PayloadMode::Sms,
                batch: Vec::new(),
                finalizer: None,
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

    async fn dial_with_release_retry(
        command_tx: &mpsc::Sender<hardware::AtRequest>,
        number: &str,
    ) -> String {
        const MAX_ATTEMPTS: usize = 5;
        const RELEASE_RETRY_DELAY: Duration = Duration::from_millis(500);

        let command = format!("ATD{number};");
        for attempt in 1..=MAX_ATTEMPTS {
            let response = run_actor(command_tx, command.clone(), None, false).await;
            if !is_transient_call_release_error(&response) || attempt == MAX_ATTEMPTS {
                return response;
            }
            tokio::time::sleep(RELEASE_RETRY_DELAY).await;
        }
        unreachable!("the bounded dial loop always returns")
    }

    fn is_transient_call_release_error(response: &str) -> bool {
        let response = response.to_ascii_lowercase();
        response.contains("+cme error:") && response.contains("operation not allowed")
    }

    async fn check_viettel_balance(
        command_tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
    ) -> Result<(String, String), String> {
        use std::collections::HashSet;
        let baseline = actor_pdu_snapshot(command_tx).await?;
        let baseline_records = snapshot_records(modemd::sms::parse_cmgl(&baseline), now());
        let identities: HashSet<String> = baseline_records
            .iter()
            .map(|record| record.fingerprint.clone())
            .collect();

        let submitted =
            actor_lines(command_tx, "AT+CMGS=\"191\"".into(), Some(b"TK".to_vec())).await?;
        if !submitted.iter().any(|line| line.starts_with("+CMGS:")) {
            return Err("modem returned OK without accepting the TK submission".into());
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            match actor_pdu_snapshot(command_tx).await {
                Ok(lines) => {
                    let stamp = now();
                    let records = snapshot_records(modemd::sms::parse_cmgl(&lines), stamp);
                    if let Some(message) = records.iter().find(|record| {
                        !identities.contains(&record.fingerprint)
                            && record.peer == "191"
                            && record.multipart_complete
                            && is_viettel_balance_body(&record.body)
                    }) {
                        store
                            .sync_sms(&records, stamp)
                            .map_err(|error| error.to_string())?;
                        let canonical = store
                            .list_sms(1000)
                            .map_err(|error| error.to_string())?
                            .into_iter()
                            .find(|record| {
                                record.source == "sim" && record.fingerprint == message.fingerprint
                            })
                            .ok_or_else(|| {
                                "balance SMS was synchronized but could not be linked".to_owned()
                            })?;
                        return Ok((message.body.clone(), canonical.id));
                    }
                }
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err("timed out waiting for a new complete Viettel balance SMS from 191".into())
    }

    fn is_viettel_balance_body(body: &str) -> bool {
        let folded = body
            .to_lowercase()
            .replace(
                [
                    'á', 'à', 'ả', 'ã', 'ạ', 'ă', 'ắ', 'ằ', 'ẳ', 'ẵ', 'ặ', 'â', 'ấ', 'ầ', 'ẩ', 'ẫ',
                    'ậ',
                ],
                "a",
            )
            .replace(
                ['ố', 'ồ', 'ổ', 'ỗ', 'ộ', 'ô', 'ớ', 'ờ', 'ở', 'ỡ', 'ợ', 'ơ'],
                "o",
            )
            .replace(['ố'], "o")
            .replace(['ư', 'ứ', 'ừ', 'ử', 'ữ', 'ự'], "u")
            .replace('đ', "d");
        folded.contains("tk goc") || folded.contains("tai khoan") || folded.contains("so du")
    }

    #[cfg(test)]
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
        let dcs = csv_field(parts[header], 8).and_then(parse_dcs);
        let compact_hex: String = body
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let decoded = decode_ucs2_hex(&compact_hex);
        let is_ucs2 = dcs.is_some_and(dcs_uses_ucs2)
            || (dcs.is_none() && decoded.as_deref().is_some_and(looks_like_text));
        Some(if is_ucs2 {
            // A malformed or truncated UCS2 response should still be visible to the
            // caller instead of making the already-read SMS disappear.
            decoded.unwrap_or(body)
        } else {
            body
        })
    }

    #[cfg(test)]
    fn parse_dcs(value: &str) -> Option<u8> {
        let value = value.trim().trim_matches('"');
        value.parse().ok().or_else(|| {
            value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        })
    }

    #[cfg(test)]
    fn looks_like_text(value: &str) -> bool {
        let value = value.strip_prefix('\u{feff}').unwrap_or(value);
        !value.is_empty()
            && value.chars().any(|character| character.is_alphabetic())
            && value
                .chars()
                .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
    }

    #[cfg(test)]
    fn dcs_uses_ucs2(dcs: u8) -> bool {
        (dcs & 0xc0 == 0 && dcs & 0x0c == 0x08) || dcs & 0xf0 == 0xe0
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
            .map(|decoded| {
                decoded
                    .strip_prefix('\u{feff}')
                    .unwrap_or(&decoded)
                    .to_owned()
            })
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
        use super::{
            is_transient_call_release_error, is_viettel_balance_body, modem_timestamp_ms,
            viettel_balance_message,
        };

        #[test]
        fn modem_timestamp_uses_service_centre_timezone() {
            assert_eq!(
                modem_timestamp_ms("26/08/03,10:00:00+28"),
                Some(1_785_726_000_000)
            );
            assert!(modem_timestamp_ms("invalid").is_none());
        }

        #[test]
        fn balance_indicators_exclude_unrelated_191_notifications() {
            assert!(is_viettel_balance_body("TK gốc: 85.500đ"));
            assert!(is_viettel_balance_body("Số dư tài khoản của quý khách"));
            assert!(!is_viettel_balance_body(
                "Quý khách đã đăng ký VoLTE thành công"
            ));
        }

        #[test]
        fn retries_dial_when_previous_call_is_still_releasing() {
            assert!(is_transient_call_release_error(
                "ERROR: modem rejected command: +CME ERROR: operation not allowed\n"
            ));
        }

        #[test]
        fn does_not_retry_other_dial_failures() {
            assert!(!is_transient_call_release_error(
                "ERROR: modem rejected command: +CME ERROR: SIM not inserted\n"
            ));
            assert!(!is_transient_call_release_error(
                "ERROR: command timed out\n"
            ));
        }

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

        #[test]
        fn decodes_multiline_ucs2_without_extended_header() {
            let response = "+CMGL: 11,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\" | FEFF00540068007500EA002000620061006F003A | 002000310030003000300030003000200056004E0044 | OK";
            assert_eq!(
                viettel_balance_message(response).unwrap(),
                "Thuê bao: 100000 VND"
            );
        }

        #[test]
        fn accepts_quoted_hex_dcs() {
            let response = "+CMGL: 12,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,\"0x08\" | 00540068007500EA | OK";
            assert_eq!(viettel_balance_message(response).unwrap(), "Thuê");
        }

        #[test]
        fn preserves_malformed_ucs2_instead_of_losing_message() {
            let response = "+CMGL: 13,\"REC UNREAD\",\"191\",\"\",\"26/08/03,10:00:00+28\",129,0,0,8 | 0054006 | OK";
            assert_eq!(viettel_balance_message(response).unwrap(), "0054006");
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
