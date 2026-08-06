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

    #[derive(Clone, Debug, Default)]
    struct DeliveryCapability {
        attempted: bool,
        report_request_available: bool,
        report_reception_available: bool,
        available: bool,
        error: String,
    }

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
        let delivery_capability = Arc::new(RwLock::new(DeliveryCapability::default()));
        let delivery_configuration = Arc::new(tokio::sync::Mutex::new(()));
        let monitor_state = Arc::clone(&device_state);
        let monitor_capability = Arc::clone(&delivery_capability);
        let monitor_stop = Arc::new(AtomicBool::new(false));
        let monitor_stop_task = Arc::clone(&monitor_stop);
        let monitor_settings = Arc::clone(&settings);
        let (command_tx, command_rx) = mpsc::channel();
        let (sms_event_tx, sms_event_rx) = mpsc::channel();
        let monitor = tokio::task::spawn_blocking(move || {
            hardware::monitor_with_commands(
                monitor_settings
                    .read()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .clone(),
                monitor_stop_task,
                |state| {
                    *monitor_capability
                        .write()
                        .unwrap_or_else(|lock| lock.into_inner()) = DeliveryCapability::default();
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
                sms_event_tx,
            );
        });
        let call_manager = Arc::new(CallManager::new(
            command_tx.clone(),
            Arc::clone(&store),
            Arc::clone(&settings),
        ));
        let sync_tx = command_tx.clone();
        let sync_store = Arc::clone(&store);
        let sync_calls = Arc::clone(&call_manager);
        let sync_capability = Arc::clone(&delivery_capability);
        let sync_configuration = Arc::clone(&delivery_configuration);
        let sync_device_state = Arc::clone(&device_state);
        let sms_synchronizer = tokio::spawn(async move {
            let mut next_reconciliation = tokio::time::Instant::now();
            let mut next_configuration_attempt = tokio::time::Instant::now();
            let mut stored_event_due = None;
            let mut pending_direct_reports = Vec::new();
            loop {
                let current = tokio::time::Instant::now();
                let modem_ready = matches!(
                    &*sync_device_state
                        .read()
                        .unwrap_or_else(|lock| lock.into_inner()),
                    hardware::HardwareState::Ready { .. }
                );
                let needs_configuration = {
                    let capability = sync_capability
                        .read()
                        .unwrap_or_else(|lock| lock.into_inner());
                    !capability.attempted
                        || (!capability.available && current >= next_configuration_attempt)
                };
                if modem_ready && sync_calls.sms_sync_allowed() && needs_configuration {
                    let _configuration = sync_configuration.lock().await;
                    let still_needs_configuration = {
                        let capability = sync_capability
                            .read()
                            .unwrap_or_else(|lock| lock.into_inner());
                        !capability.attempted || !capability.available
                    };
                    if still_needs_configuration {
                        let capability = configure_delivery_tracking(&sync_tx).await;
                        *sync_capability
                            .write()
                            .unwrap_or_else(|lock| lock.into_inner()) = capability;
                    }
                    next_configuration_attempt =
                        tokio::time::Instant::now() + Duration::from_secs(10);
                }
                while let Ok(event) = sms_event_rx.try_recv() {
                    match event {
                        hardware::SmsUrcEvent::DirectReport(lines) => {
                            let stamp = now();
                            for report in snapshot_records(modemd::sms::parse_cmgl(&lines), stamp) {
                                if report.kind == "status-report" {
                                    pending_direct_reports.push((report, current));
                                }
                            }
                        }
                        hardware::SmsUrcEvent::StoredIndication(_) => {
                            stored_event_due = Some(current + Duration::from_millis(500));
                        }
                    }
                }
                let mut retry_reports = Vec::new();
                for (report, first_seen) in pending_direct_reports.drain(..) {
                    match sync_store.apply_direct_delivery_report(&report, now()) {
                        Ok(true) => {}
                        Ok(false)
                            if current.duration_since(first_seen) < Duration::from_secs(300) =>
                        {
                            retry_reports.push((report, first_seen));
                        }
                        Ok(false) => {
                            eprintln!("direct SMS delivery report could not be correlated")
                        }
                        Err(error) => {
                            eprintln!("direct SMS delivery report deferred: {error}");
                            retry_reports.push((report, first_seen));
                        }
                    }
                }
                pending_direct_reports = retry_reports;
                let event_due = stored_event_due.is_some_and(|due| current >= due);
                if sync_calls.sms_sync_allowed() && (current >= next_reconciliation || event_due) {
                    if let Err(error) = sync_sms_json(&sync_tx, &sync_store).await {
                        eprintln!("automatic SMS synchronization deferred: {error}");
                    }
                    let _ = sync_store.expire_delivery_reports(now());
                    next_reconciliation = tokio::time::Instant::now() + Duration::from_secs(300);
                    stored_event_due = None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
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
                    sms_synchronizer.abort();
                    monitor_stop.store(true, Ordering::Relaxed);
                    monitor.await.map_err(io::Error::other)?;
                    return Ok(());
                },
                connected = server.connect() => {
                    connected?;
                    tokio::spawn(handle_client(server, Arc::clone(&device_state), Arc::clone(&delivery_capability), Arc::clone(&delivery_configuration), command_tx.clone(), Arc::clone(&store), Arc::clone(&call_manager)));
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
        delivery_capability: Arc<RwLock<DeliveryCapability>>,
        delivery_configuration: Arc<tokio::sync::Mutex<()>>,
        command_tx: mpsc::Sender<hardware::AtRequest>,
        store: Arc<Store>,
        call_manager: Arc<CallManager>,
    ) -> io::Result<()> {
        let mut stream = BufReader::new(server);
        let mut line = String::new();
        while stream.read_line(&mut line).await? != 0 {
            let request = line.trim();
            let response = if request.starts_with('{') {
                handle_json(
                    request,
                    &command_tx,
                    &store,
                    &call_manager,
                    &delivery_capability,
                    &delivery_configuration,
                )
                .await
            } else if request == "STATUS" {
                let state = device_state.read().unwrap_or_else(|lock| lock.into_inner());
                let state = match &*state {
                    hardware::HardwareState::Ready { port_name } => format!("Ready\t{port_name}"),
                    hardware::HardwareState::PortBusy { port_name } => {
                        format!("Port busy\t{port_name}")
                    }
                    hardware::HardwareState::Disconnected => "Disconnected\t".to_owned(),
                };
                let capability = delivery_capability
                    .read()
                    .unwrap_or_else(|lock| lock.into_inner());
                format!(
                    "STATUS\t0.1.0\t{state}\tUNKNOWN\tNot registered\t0\t{}\t{}\n",
                    capability.available,
                    capability.error.replace(['\t', '\r', '\n'], " ")
                )
            } else if let Some(rest) = request.strip_prefix("SMS|") {
                let mut fields = rest.splitn(2, '|');
                let destination = fields.next().unwrap_or_default();
                let body = fields.next().and_then(decode_hex);
                match (modemd::sms::normalize_sms_destination(destination), body) {
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
                match modemd::sms::normalize_call_destination(number) {
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
        delivery_capability: &Arc<RwLock<DeliveryCapability>>,
        delivery_configuration: &Arc<tokio::sync::Mutex<()>>,
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
            "send_sms" => send_sms_json(
                value,
                tx,
                store,
                delivery_capability,
                delivery_configuration,
            )
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
        let destination = modemd::sms::normalize_call_destination(
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
        delivery_capability: &Arc<RwLock<DeliveryCapability>>,
        delivery_configuration: &Arc<tokio::sync::Mutex<()>>,
    ) -> Result<SmsRecord, String> {
        let peer = modemd::sms::normalize_sms_destination(
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
        if !delivery_capability
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .attempted
        {
            let _configuration = delivery_configuration.lock().await;
            let still_needs_configuration = !delivery_capability
                .read()
                .unwrap_or_else(|lock| lock.into_inner())
                .attempted;
            if still_needs_configuration {
                let capability = configure_delivery_tracking(tx).await;
                *delivery_capability
                    .write()
                    .unwrap_or_else(|lock| lock.into_inner()) = capability;
            }
        }
        let capability = delivery_capability
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone();
        record.delivery_report_requested = capability.report_request_available;
        record.delivery_tracking_error = capability.error.clone();
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
        record.state =
            if capability.report_request_available && capability.report_reception_available {
                "delivery-pending"
            } else {
                "submitted"
            }
            .into();
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
        tokio::time::timeout(Duration::from_secs(45), rx)
            .await
            .map_err(|_| "modem command timed out".to_owned())?
            .map_err(|_| "modem command actor stopped".to_owned())?
            .map_err(|e| e.to_string())
    }

    async fn configure_delivery_tracking(
        tx: &mpsc::Sender<hardware::AtRequest>,
    ) -> DeliveryCapability {
        let mut general_errors = Vec::new();
        for (label, command) in [
            ("CMGF", "AT+CMGF=1"),
            ("CPMS", "AT+CPMS=\"SM\",\"SM\",\"SM\""),
        ] {
            if actor_lines(tx, command.into(), None).await.is_err() {
                general_errors.push(format!("{label} configuration failed"));
            }
        }
        let report_request = configure_and_verify(
            tx,
            "CSMP",
            "AT+CSMP=49,167,0,0",
            "AT+CSMP?",
            "+CSMP:49,167,0,0",
        )
        .await;
        let report_request_available = report_request.is_ok();
        let direct_report = configure_and_verify(
            tx,
            "CNMI",
            "AT+CNMI=2,1,0,1,0",
            "AT+CNMI?",
            "+CNMI:2,1,0,1,0",
        )
        .await;
        let direct_report_reception = direct_report.is_ok();
        let stored_report_reception = if direct_report_reception {
            Ok(())
        } else {
            configure_and_verify(
                tx,
                "CNMI",
                "AT+CNMI=2,1,0,2,0",
                "AT+CNMI?",
                "+CNMI:2,1,0,2,0",
            )
            .await
        };
        let report_reception_available = direct_report_reception || stored_report_reception.is_ok();
        let mut errors = general_errors;
        if let Err(error) = report_request {
            errors.push(error);
        }
        if !direct_report_reception && stored_report_reception.is_ok() {
            let reason = direct_report
                .err()
                .unwrap_or_else(|| "CNMI direct delivery reports could not be verified".into());
            errors.push(format!(
                "{reason}; using stored-report synchronization (modem storage must have free slots)"
            ));
        } else if !report_reception_available {
            errors.push(
                stored_report_reception.err().unwrap_or_else(|| {
                    "CNMI delivery-report reception could not be verified".into()
                }),
            );
        }
        DeliveryCapability {
            attempted: true,
            report_request_available,
            report_reception_available,
            available: report_request_available && report_reception_available,
            error: errors.join("; "),
        }
    }

    async fn configure_and_verify(
        tx: &mpsc::Sender<hardware::AtRequest>,
        _label: &str,
        set_command: &str,
        query_command: &str,
        expected: &str,
    ) -> Result<(), String> {
        let mut last_error = format!("{_label} configuration could not be verified");
        for attempt in 0..3 {
            match actor_lines(tx, set_command.into(), None).await {
                Ok(_) => {}
                Err(error) => {
                    last_error = format!("{_label} configuration failed: {error}");
                    continue;
                }
            }
            match actor_lines(tx, query_command.into(), None).await {
                Ok(lines) if lines.iter().any(|line| setting_matches(line, expected)) => {
                    return Ok(());
                }
                Ok(lines) => {
                    let readback = lines.join(" | ");
                    last_error = if readback.is_empty() {
                        format!("{_label} readback was empty")
                    } else {
                        format!("{_label} readback did not match: {readback}")
                    };
                }
                Err(error) => last_error = format!("{_label} readback failed: {error}"),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Err(last_error)
    }

    fn setting_matches(line: &str, expected: &str) -> bool {
        line.chars()
            .filter(|character| !character.is_ascii_whitespace())
            .eq(expected
                .chars()
                .filter(|character| !character.is_ascii_whitespace()))
    }
    async fn sync_sms_json(
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
    ) -> Result<usize, String> {
        let lines = actor_pdu_snapshot(tx).await?;
        let stamp = now();
        let records = snapshot_records(modemd::sms::parse_cmgl(&lines), stamp);
        store.sync_sms(&records, stamp).map_err(|e| e.to_string())?;
        archive_modem_sms(tx, store, &records, stamp).await?;
        Ok(records.len())
    }

    async fn archive_modem_sms(
        tx: &mpsc::Sender<hardware::AtRequest>,
        store: &Store,
        records: &[SmsRecord],
        now_ms: i64,
    ) -> Result<(), String> {
        let archived_records = records
            .iter()
            .filter(|record| sms_ready_for_archive(record, now_ms))
            .cloned()
            .collect::<Vec<_>>();
        let commands = archive_commands(&archived_records, now_ms);
        if commands.is_empty() {
            return Ok(());
        }
        let count = commands.len();
        actor_batch_lines(tx, commands, None, Duration::from_secs(90)).await?;
        store
            .mark_sms_archived(&archived_records)
            .map_err(|error| error.to_string())?;
        eprintln!("archived {count} modem SMS storage slots after durable synchronization");
        Ok(())
    }

    const MULTIPART_ARCHIVE_GRACE_MS: i64 = 5 * 60 * 1000;

    fn sms_ready_for_archive(record: &SmsRecord, now_ms: i64) -> bool {
        record.part_count <= 1
            || record.multipart_complete
            || now_ms.saturating_sub(record.created_at_ms) >= MULTIPART_ARCHIVE_GRACE_MS
    }

    fn archive_commands(records: &[SmsRecord], now_ms: i64) -> Vec<String> {
        let mut indices = records
            .iter()
            .filter(|record| sms_ready_for_archive(record, now_ms))
            .flat_map(|record| record.storage_indices.iter().copied())
            .filter(|index| *index > 0)
            .collect::<Vec<_>>();
        indices.sort_unstable_by(|left, right| right.cmp(left));
        indices.dedup();
        indices
            .into_iter()
            .map(|index| format!("AT+CMGD={index}"))
            .collect()
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
                    modem_timestamp: x.modem_timestamp.clone(),
                    encoding: x.encoding,
                    dcs: x.dcs,
                    length: x.length,
                    service_center: x.service_center,
                    delivery_status: x.delivery_status,
                    delivery_report_scts: x.modem_timestamp.clone(),
                    delivery_report_discharge_time: x.discharge_time,
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
        actor_batch_lines(
            tx,
            vec![
                "AT+CPMS=\"SM\",\"SM\",\"SM\"".into(),
                "AT+CMGF=0".into(),
                "AT+CMGL=4".into(),
            ],
            Some("AT+CMGF=1".into()),
            Duration::from_secs(35),
        )
        .await
    }

    async fn actor_batch_lines(
        tx: &mpsc::Sender<hardware::AtRequest>,
        batch: Vec<String>,
        finalizer: Option<String>,
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(hardware::AtRequest {
            command: String::new(),
            payload: None,
            guarded: false,
            payload_mode: PayloadMode::Sms,
            batch,
            finalizer,
            reply,
        })
        .map_err(|_| "modem command actor unavailable".to_owned())?;
        tokio::time::timeout(timeout, rx)
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
        let persisted_baseline = store
            .list_sms(usize::MAX)
            .map_err(|error| error.to_string())?;
        let identities: HashSet<String> = persisted_baseline
            .iter()
            .chain(&baseline_records)
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
                    if find_balance_candidate(&records, &identities).is_some() {
                        store
                            .sync_sms(&records, stamp)
                            .map_err(|error| error.to_string())?;
                    }
                }
                Err(error) => return Err(error),
            }
            if let Some(message) = find_persisted_balance_candidate(store, &identities)? {
                return Ok((message.body, message.id));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if let Some(message) = find_persisted_balance_candidate(store, &identities)? {
            return Ok((message.body, message.id));
        }
        Err("timed out waiting for a new complete Viettel balance SMS from 191".into())
    }

    fn find_persisted_balance_candidate(
        store: &Store,
        baseline: &std::collections::HashSet<String>,
    ) -> Result<Option<SmsRecord>, String> {
        let records = store
            .list_sms(usize::MAX)
            .map_err(|error| error.to_string())?;
        Ok(find_balance_candidate(&records, baseline).cloned())
    }

    fn find_balance_candidate<'a>(
        records: &'a [SmsRecord],
        baseline: &std::collections::HashSet<String>,
    ) -> Option<&'a SmsRecord> {
        records.iter().find(|record| {
            !baseline.contains(&record.fingerprint)
                && record.direction == "inbound"
                && record.peer == "191"
                && record.multipart_complete
                && is_viettel_balance_body(&record.body)
        })
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
            MULTIPART_ARCHIVE_GRACE_MS, archive_commands, find_balance_candidate,
            find_persisted_balance_candidate, is_transient_call_release_error,
            is_viettel_balance_body, modem_timestamp_ms, setting_matches, viettel_balance_message,
        };
        use modemd::storage::{BalanceRecord, SmsRecord, Store};
        use std::collections::HashSet;

        #[test]
        fn delivery_configuration_readback_ignores_modem_whitespace() {
            assert!(setting_matches("+CNMI: 2,1,0,1,0", "+CNMI:2,1,0,1,0"));
            assert!(!setting_matches("+CNMI: 2,1,0,2,0", "+CNMI:2,1,0,1,0"));
        }

        #[test]
        fn archive_commands_delete_each_persisted_slot_once_in_descending_order() {
            let records = vec![
                SmsRecord {
                    storage_indices: vec![1, 3],
                    multipart_complete: true,
                    ..Default::default()
                },
                SmsRecord {
                    storage_indices: vec![3, -1, 2],
                    multipart_complete: true,
                    ..Default::default()
                },
            ];
            assert_eq!(
                archive_commands(&records, 0),
                ["AT+CMGD=3", "AT+CMGD=2", "AT+CMGD=1"]
            );
        }

        #[test]
        fn archive_commands_give_incomplete_multipart_sms_time_to_finish() {
            let received_at = 1_000_000;
            let incomplete = SmsRecord {
                storage_indices: vec![1, 2, 3],
                part_count: 4,
                parts_received: 3,
                multipart_complete: false,
                created_at_ms: received_at,
                ..Default::default()
            };
            assert!(
                archive_commands(
                    std::slice::from_ref(&incomplete),
                    received_at + MULTIPART_ARCHIVE_GRACE_MS - 1,
                )
                .is_empty()
            );
            assert_eq!(
                archive_commands(
                    std::slice::from_ref(&incomplete),
                    received_at + MULTIPART_ARCHIVE_GRACE_MS,
                ),
                ["AT+CMGD=3", "AT+CMGD=2", "AT+CMGD=1"]
            );

            let complete = SmsRecord {
                multipart_complete: true,
                ..incomplete
            };
            assert_eq!(
                archive_commands(&[complete], received_at),
                ["AT+CMGD=3", "AT+CMGD=2", "AT+CMGD=1"]
            );
        }

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

        fn balance_sms(id: &str) -> SmsRecord {
            SmsRecord {
                id: id.into(),
                direction: "inbound".into(),
                peer: "191".into(),
                body: "TK gốc: 85.500đ".into(),
                source: "sim".into(),
                storage: "SM".into(),
                fingerprint: id.into(),
                multipart_complete: true,
                ..Default::default()
            }
        }

        #[test]
        fn new_modem_resident_balance_reply_is_a_candidate() {
            let reply = balance_sms("new-reply");
            assert_eq!(
                find_balance_candidate(&[reply], &HashSet::new()).map(|record| record.id.as_str()),
                Some("new-reply")
            );
        }

        #[test]
        fn archived_persisted_balance_reply_is_found_and_linked() {
            let store = Store::memory().unwrap();
            let mut reply = balance_sms("canonical-sms");
            store.sync_sms(std::slice::from_ref(&reply), 10).unwrap();
            store
                .mark_sms_archived(std::slice::from_ref(&reply))
                .unwrap();
            reply.present_on_modem = false;

            let found = find_persisted_balance_candidate(&store, &HashSet::new())
                .unwrap()
                .unwrap();
            assert_eq!(found.id, "canonical-sms");
            assert!(!found.present_on_modem);
        }

        #[test]
        fn pre_existing_balance_reply_cannot_satisfy_a_new_request() {
            let reply = balance_sms("old-reply");
            let baseline = HashSet::from([reply.fingerprint.clone()]);
            assert!(find_balance_candidate(&[reply], &baseline).is_none());
        }

        #[test]
        fn balance_candidate_rejects_wrong_sender_unrelated_body_and_incomplete_multipart() {
            let mut wrong_sender = balance_sms("wrong-sender");
            wrong_sender.peer = "123".into();
            let mut outbound = balance_sms("outbound");
            outbound.direction = "outbound".into();
            let mut unrelated = balance_sms("unrelated");
            unrelated.body = "Quy khach da dang ky VoLTE thanh cong".into();
            let mut incomplete = balance_sms("incomplete");
            incomplete.part_count = 2;
            incomplete.parts_received = 1;
            incomplete.multipart_complete = false;
            assert!(
                find_balance_candidate(
                    &[wrong_sender, outbound, unrelated, incomplete],
                    &HashSet::new()
                )
                .is_none()
            );
        }

        #[test]
        fn final_persisted_check_catches_reply_archived_at_deadline() {
            let store = Store::memory().unwrap();
            let reply = balance_sms("deadline-reply");
            assert!(
                find_persisted_balance_candidate(&store, &HashSet::new())
                    .unwrap()
                    .is_none()
            );
            store
                .sync_sms(std::slice::from_ref(&reply), 30_000)
                .unwrap();
            store
                .mark_sms_archived(std::slice::from_ref(&reply))
                .unwrap();

            let found = find_persisted_balance_candidate(&store, &HashSet::new())
                .unwrap()
                .unwrap();
            assert_eq!(found.id, "deadline-reply");
        }

        #[test]
        fn balance_result_uses_one_canonical_sms_and_one_history_record() {
            let store = Store::memory().unwrap();
            let reply = balance_sms("canonical-reply");
            store.sync_sms(std::slice::from_ref(&reply), 1).unwrap();
            store.sync_sms(std::slice::from_ref(&reply), 2).unwrap();
            let canonical = find_persisted_balance_candidate(&store, &HashSet::new())
                .unwrap()
                .unwrap();
            store
                .save_balance(&BalanceRecord {
                    id: "balance-check".into(),
                    raw: canonical.body,
                    sms_id: canonical.id,
                    ..Default::default()
                })
                .unwrap();

            let sms = store.list_sms(10).unwrap();
            let balances = store.list_balances(10).unwrap();
            assert_eq!(sms.len(), 1);
            assert_eq!(balances.len(), 1);
            assert_eq!(balances[0].sms_id, sms[0].id);
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
