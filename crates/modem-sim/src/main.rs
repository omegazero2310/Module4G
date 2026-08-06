#[cfg(windows)]
mod windows_sim;

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    windows_sim::host::run().await
}

#[cfg(not(windows))]
fn main() {
    eprintln!("modem-sim named-pipe mode is available only on Windows");
}
