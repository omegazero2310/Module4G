#[cfg(windows)]
mod windows_host;

#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    windows_host::host::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("A7670ModemService is supported only on Windows.");
}
