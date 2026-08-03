#[cfg(windows)]
mod windows_sim {
    use std::io;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";

    fn response(command: &str) -> &'static str {
        match command.trim() {
            "STATUS" => "STATUS\t0.1.0-sim\tReady\tSIMULATED\tREADY\tRegistered\t20\n",
            "AT" | "AT+CMEE=2" | "AT+CVHU=0" | "AT+CLCC=1" | "AT+CMGF=1" | "AT+CNMI=1,1,0,1,0" => {
                "OK\n"
            }
            "AT+CSQ" => "+CSQ: 20,99\r\nOK\n",
            "AT+CPIN?" => "+CPIN: READY\r\nOK\n",
            "AT+CREG?" => "+CREG: 0,1\r\nOK\n",
            _ => "ERROR\n",
        }
    }

    async fn serve_connection(
        server: tokio::net::windows::named_pipe::NamedPipeServer,
    ) -> io::Result<()> {
        server.connect().await?;
        let mut line = String::new();
        let mut stream = BufReader::new(server);
        while stream.read_line(&mut line).await? != 0 {
            stream
                .get_mut()
                .write_all(response(&line).as_bytes())
                .await?;
            stream.get_mut().flush().await?;
            line.clear();
        }
        Ok(())
    }

    pub async fn run() -> io::Result<()> {
        eprintln!("modem-sim listening on {PIPE}");
        loop {
            let server = ServerOptions::new().create(PIPE)?;
            if let Err(error) = serve_connection(server).await {
                eprintln!("simulator client error: {error}");
            }
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    windows_sim::run().await
}

#[cfg(not(windows))]
fn main() {
    eprintln!("modem-sim named-pipe mode is available only on Windows");
}
