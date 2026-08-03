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
        let monitor = tokio::task::spawn_blocking(move || {
            hardware::monitor(Settings::default(), monitor_stop_task, |state| {
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
            });
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
                    tokio::spawn(handle_client(server, Arc::clone(&device_state)));
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
    ) -> io::Result<()> {
        let mut stream = BufReader::new(server);
        let mut line = String::new();
        while stream.read_line(&mut line).await? != 0 {
            let response = if line.trim() == "STATUS" {
                let state = device_state.read().unwrap_or_else(|lock| lock.into_inner());
                let state = match &*state {
                    hardware::HardwareState::Ready { port_name } => format!("Ready\t{port_name}"),
                    hardware::HardwareState::PortBusy { port_name } => {
                        format!("Port busy\t{port_name}")
                    }
                    hardware::HardwareState::Disconnected => "Disconnected\t".to_owned(),
                };
                format!("STATUS\t0.1.0\t{state}\tUNKNOWN\tNot registered\t0\n")
            } else {
                "ERROR\n".to_owned()
            };
            stream.get_mut().write_all(response.as_bytes()).await?;
            stream.get_mut().flush().await?;
            line.clear();
        }
        Ok(())
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
