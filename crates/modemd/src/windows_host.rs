#[cfg(windows)]
pub mod host {
    mod balance;
    mod routing;
    mod sms_workflow;
    use balance::*;
    use modemd::{
        call_workflow::CallManager,
        hardware::{self, PayloadMode},
        integration::{self, CommunicationDispatcher, DispatchError, RestState},
        settings::Settings,
        storage::{BalanceRecord, IntegrationSettings, SmsRecord, Store},
    };
    use routing::*;
    use sms_workflow::*;
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

    #[derive(Clone)]
    struct RuntimeContext {
        device_state: Arc<RwLock<hardware::HardwareState>>,
        delivery_capability: Arc<RwLock<DeliveryCapability>>,
        delivery_configuration: Arc<tokio::sync::Mutex<()>>,
        command_tx: mpsc::Sender<hardware::AtRequest>,
        store: Arc<Store>,
        call_manager: Arc<CallManager>,
        integration_settings: Arc<RwLock<IntegrationSettings>>,
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
        let integration_settings = Arc::new(RwLock::new(
            store
                .load_integration_settings()
                .map_err(io::Error::other)?,
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
            // Drop the read guard before entering the lifetime-long monitor.
            // Passing read().clone() directly as an argument extends the guard
            // through the call and permanently blocks settings updates.
            let initial_settings = settings_snapshot(&monitor_settings);
            hardware::monitor_with_commands(
                initial_settings,
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
        let context = Arc::new(RuntimeContext {
            device_state: Arc::clone(&device_state),
            delivery_capability: Arc::clone(&delivery_capability),
            delivery_configuration: Arc::clone(&delivery_configuration),
            command_tx: command_tx.clone(),
            store: Arc::clone(&store),
            call_manager: Arc::clone(&call_manager),
            integration_settings: Arc::clone(&integration_settings),
        });
        let rest_dispatcher: Arc<dyn CommunicationDispatcher> = Arc::new(HostDispatcher {
            command_tx: command_tx.clone(),
            store: Arc::clone(&store),
            call_manager: Arc::clone(&call_manager),
            delivery_capability: Arc::clone(&delivery_capability),
            delivery_configuration: Arc::clone(&delivery_configuration),
        });
        let rest_state = RestState {
            store: Arc::clone(&store),
            settings: Arc::clone(&integration_settings),
            dispatcher: rest_dispatcher,
        };
        let bind_address = integration_settings
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .rest_bind_address
            .clone();
        let rest_listener = tokio::spawn(async move {
            loop {
                match tokio::net::TcpListener::bind(&bind_address).await {
                    Ok(listener) => {
                        eprintln!("REST integration listener bound on {bind_address}");
                        if let Err(error) = integration::serve(listener, rest_state.clone()).await {
                            eprintln!("REST integration listener stopped: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("REST integration bind deferred on {bind_address}: {error}")
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        let webhook_worker = tokio::spawn(integration::deliver_webhooks(
            Arc::clone(&store),
            Arc::clone(&integration_settings),
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
                    rest_listener.abort();
                    webhook_worker.abort();
                    monitor_stop.store(true, Ordering::Relaxed);
                    monitor.await.map_err(io::Error::other)?;
                    return Ok(());
                },
                connected = server.connect() => {
                    connected?;
                    tokio::spawn(handle_client(server, Arc::clone(&context)));
                }
            }
        }
    }

    fn settings_snapshot(settings: &RwLock<Settings>) -> Settings {
        settings
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    struct HostDispatcher {
        command_tx: mpsc::Sender<hardware::AtRequest>,
        store: Arc<Store>,
        call_manager: Arc<CallManager>,
        delivery_capability: Arc<RwLock<DeliveryCapability>>,
        delivery_configuration: Arc<tokio::sync::Mutex<()>>,
    }

    #[async_trait::async_trait]
    impl CommunicationDispatcher for HostDispatcher {
        async fn send_sms(
            &self,
            id: String,
            destination: String,
            body: String,
        ) -> Result<(), DispatchError> {
            let value = serde_json::json!({"destination":destination,"body":body});
            send_sms_with_id(
                id,
                value,
                &self.command_tx,
                &self.store,
                &self.delivery_capability,
                &self.delivery_configuration,
            )
            .await
            // The SMS workflow persists both confirmed rejection and unknown
            // submission outcomes. Reconciliation maps that durable state.
            .map(|_| ())
            .or(Ok(()))
        }
        async fn make_call(
            &self,
            id: String,
            destination: String,
            audio_id: String,
        ) -> Result<(), DispatchError> {
            self.call_manager
                .select_audio(&audio_id)
                .map_err(|error| match error {
                    modemd::ModemError::Validation(message) => DispatchError::Validation(message),
                    _ => DispatchError::Unavailable(error.to_string()),
                })?;
            self.call_manager
                .make_call_with_id(id, destination, audio_id)
                .await
                .map(|_| ())
                .map_err(|error| match error {
                    modemd::ModemError::Validation(message) => DispatchError::Validation(message),
                    modemd::ModemError::Busy | modemd::ModemError::Disconnected => {
                        DispatchError::Unavailable(error.to_string())
                    }
                    _ => DispatchError::Failed(error.to_string()),
                })
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

    #[cfg(test)]
    mod tests {
        use super::{
            MULTIPART_ARCHIVE_GRACE_MS, archive_commands, find_balance_candidate,
            find_persisted_balance_candidate, is_transient_call_release_error,
            is_viettel_balance_body, modem_timestamp_ms, setting_matches, settings_snapshot,
            viettel_balance_message,
        };
        use modemd::settings::Settings;
        use modemd::storage::{BalanceRecord, SmsRecord, Store};
        use std::{collections::HashSet, sync::RwLock};

        #[test]
        fn monitor_settings_snapshot_releases_the_read_lock() {
            let settings = RwLock::new(Settings::default());
            let _snapshot = settings_snapshot(&settings);
            assert!(settings.try_write().is_ok());
        }

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
                "Thuê bao: 84XXXXXXXXX (HISCL): \n- TK gốc: 19.800đ, HSD: 00:00:0"
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
