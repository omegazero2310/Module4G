#[cfg(windows)]
mod windows_sim {
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";

    #[derive(Clone, Copy, Default)]
    enum CallScenario {
        #[default]
        Answered,
        NoAnswer,
        Busy,
        Unreachable,
        NetworkError,
        MissingTerminal,
    }

    fn response(command: &str, scenario: &mut CallScenario) -> &'static str {
        let command = command.trim();
        if command.starts_with("SMS|") {
            return "+CMGS: 42\r\nOK\n";
        }
        if command == "BALANCE" {
            return "Thuê bao: 84XXXXXXXXX (HISCL):\r\n- TK gốc: 85.500đ, HSD: 00:00:00 02-10-2026.\r\n- TK tiền di động: 863đ, HSD: 00:00:00 01-01-2100.\r\n- TK tiền khuyến mại: 89.174đ.\r\nĐể tìm hiểu và đăng ký các gói data ưu đãi, truy cập https://vietteltelecom.vn/goidatahot\n";
        }
        if command.starts_with("USSD|") {
            return "+CUSD: 0,\"Balance 125.50 THB\",15\r\nOK\n";
        }
        if let Some(number) = command.strip_prefix("DIAL|") {
            *scenario = match &number[number.len().saturating_sub(2)..] {
                "02" => CallScenario::NoAnswer,
                "03" => CallScenario::Busy,
                "04" => CallScenario::Unreachable,
                "05" => CallScenario::NetworkError,
                "06" => CallScenario::MissingTerminal,
                _ => CallScenario::Answered,
            };
            return "OK\n";
        }
        if command == "HANGUP" {
            return "OK\n";
        }
        if command == "CALLCAUSE" {
            return match scenario {
                CallScenario::Unreachable => "+CEER: 20 Subscriber absent\n",
                CallScenario::NetworkError => "+CEER: 34 No circuit/channel available\n",
                _ => "+CEER: 16 Normal call clearing\n",
            };
        }
        if command == "CALLSTATUS" {
            return match scenario {
                CallScenario::Answered => {
                    "+CLCC: 1,0,0,0,0 | VOICE CALL: BEGIN | VOICE CALL: END | NO CARRIER\n"
                }
                CallScenario::NoAnswer => "NO ANSWER | VOICE CALL: END | NO CARRIER\n",
                CallScenario::Busy => "BUSY | NO CARRIER\n",
                CallScenario::Unreachable | CallScenario::NetworkError => "NO CARRIER\n",
                CallScenario::MissingTerminal => "+CLCC: 1,0,2,0,0\n",
            };
        }
        match command {
            "STATUS" => "STATUS\t0.1.0-sim\tReady\tSIMULATED\tREADY\tRegistered\t20\n",
            "AT" | "AT+CMEE=2" | "AT+CVHU=0" | "AT+CLCC=1" | "AT+CMGF=1" | "AT+CNMI=1,1,0,1,0" => {
                "OK\n"
            }
            "ATI" => "SIMCOM_Ltd\r\nSIMCOM_SIM7600G-H\r\nRevision: A7670M7_V1.11\r\nOK\n",
            "AT+CSQ" => "+CSQ: 20,99\r\nOK\n",
            "AT+CPIN?" => "+CPIN: READY\r\nOK\n",
            "AT+CREG?" => "+CREG: 0,1\r\nOK\n",
            _ => "ERROR\n",
        }
    }

    async fn serve_connection(
        server: tokio::net::windows::named_pipe::NamedPipeServer,
        scenario: Arc<Mutex<CallScenario>>,
    ) -> io::Result<()> {
        server.connect().await?;
        let mut line = String::new();
        let mut stream = BufReader::new(server);
        while stream.read_line(&mut line).await? != 0 {
            stream
                .get_mut()
                .write_all(
                    response(
                        &line,
                        &mut scenario.lock().unwrap_or_else(|lock| lock.into_inner()),
                    )
                    .as_bytes(),
                )
                .await?;
            stream.get_mut().flush().await?;
            line.clear();
        }
        Ok(())
    }

    pub async fn run() -> io::Result<()> {
        eprintln!("modem-sim listening on {PIPE}");
        let scenario = Arc::new(Mutex::new(CallScenario::default()));
        loop {
            let server = ServerOptions::new().create(PIPE)?;
            if let Err(error) = serve_connection(server, Arc::clone(&scenario)).await {
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
