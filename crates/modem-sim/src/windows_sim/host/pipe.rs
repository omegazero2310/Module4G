use super::*;

pub(super) async fn serve_connection(
    server: tokio::net::windows::named_pipe::NamedPipeServer,
    state: Arc<Mutex<SimState>>,
) -> io::Result<()> {
    server.connect().await?;
    let mut line = String::new();
    let mut stream = BufReader::new(server);
    while stream.read_line(&mut line).await? != 0 {
        let reply = {
            let mut state = state.lock().unwrap_or_else(|lock| lock.into_inner());
            json_response(&line, &mut state)
                .unwrap_or_else(|| response(&line, &mut state.legacy_scenario).to_owned())
        };
        stream.get_mut().write_all(reply.as_bytes()).await?;
        stream.get_mut().flush().await?;
        line.clear();
    }
    Ok(())
}

pub async fn run() -> io::Result<()> {
    eprintln!("modem-sim listening on {PIPE}");
    let state = Arc::new(Mutex::new(SimState::default()));
    loop {
        let server = ServerOptions::new().create(PIPE)?;
        if let Err(error) = serve_connection(server, Arc::clone(&state)).await {
            eprintln!("simulator client error: {error}");
        }
    }
}
