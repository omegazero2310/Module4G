use std::mem;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Line(String),
    Prompt,
}

#[derive(Default)]
pub struct Framer {
    buffer: Vec<u8>,
}

#[derive(Default)]
pub struct Dispatcher {
    framer: Framer,
    pending_urcs: Vec<String>,
}

impl Dispatcher {
    /// Keeps one framing buffer for the lifetime of the port and separates
    /// unsolicited call notifications from the active command response.
    pub fn push(&mut self, bytes: &[u8], command: Option<&str>) -> (Vec<Frame>, Vec<String>) {
        let mut response = Vec::new();
        let mut urcs = Vec::new();
        for frame in self.framer.push(bytes) {
            match frame {
                Frame::Line(line) if crate::call::parse_urc(&line).is_some() => urcs.push(line),
                Frame::Line(line)
                    if command.is_some_and(|value| line.eq_ignore_ascii_case(value)) => {}
                other => response.push(other),
            }
        }
        self.pending_urcs.extend(urcs.iter().cloned());
        (response, urcs)
    }

    pub fn take_urcs(&mut self) -> Vec<String> {
        mem::take(&mut self.pending_urcs)
    }

    pub fn clear_urcs(&mut self) {
        self.pending_urcs.clear();
    }

    pub fn reset(&mut self) {
        self.framer.reset();
        self.pending_urcs.clear();
    }
}

impl Framer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            while self
                .buffer
                .first()
                .is_some_and(|b| *b == b'\r' || *b == b'\n')
            {
                self.buffer.remove(0);
            }
            if self.buffer.first() == Some(&b'>') {
                self.buffer.remove(0);
                if self.buffer.first() == Some(&b' ') {
                    self.buffer.remove(0);
                }
                out.push(Frame::Prompt);
                continue;
            }
            let Some(end) = self.buffer.iter().position(|b| *b == b'\r' || *b == b'\n') else {
                break;
            };
            let raw: Vec<_> = self.buffer.drain(..end).collect();
            if !raw.is_empty() {
                out.push(Frame::Line(String::from_utf8_lossy(&raw).into_owned()));
            }
        }
        out
    }
    pub fn reset(&mut self) {
        mem::take(&mut self.buffer);
    }
}

pub fn validate_console(command: &str, busy: bool) -> Result<String, crate::ModemError> {
    let command = command.trim();
    if busy {
        return Err(crate::ModemError::Busy);
    }
    if command.len() > 512 || !command.starts_with("AT") || command.chars().any(|c| c.is_control())
    {
        return Err(crate::ModemError::Validation(
            "command must start with AT, contain no controls, and be at most 512 bytes".into(),
        ));
    }
    let upper = command.to_ascii_uppercase();
    if ["CMGS", "CFTRANRX", "CMGW", "CUSD"]
        .iter()
        .any(|blocked| upper.contains(blocked))
    {
        return Err(crate::ModemError::Validation(
            "payload or interactive commands are blocked".into(),
        ));
    }
    Ok(command.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn detects_bare_prompt_and_chunked_lines() {
        let mut f = Framer::default();
        assert!(f.push(b"\r\n+CM").is_empty());
        assert_eq!(
            f.push(b"TI: \"SM\",1\r\n> "),
            vec![Frame::Line("+CMTI: \"SM\",1".into()), Frame::Prompt]
        );
    }
    #[test]
    fn console_is_guarded() {
        assert_eq!(validate_console("AT+CSQ", false).unwrap(), "AT+CSQ");
        assert!(validate_console("AT+CMGS=1", false).is_err());
        assert!(validate_console("AT\rD", false).is_err());
    }
    #[test]
    fn dispatcher_separates_interleaved_urcs() {
        let mut d = Dispatcher::default();
        let (response, urcs) = d.push(b"ATD123;\r\nVOICE CALL: BEGIN\r\nOK\r\n", Some("ATD123;"));
        assert_eq!(response, vec![Frame::Line("OK".into())]);
        assert_eq!(urcs, vec!["VOICE CALL: BEGIN"]);
        assert_eq!(d.take_urcs(), vec!["VOICE CALL: BEGIN"]);
    }
    proptest::proptest! {
        #[test] fn arbitrary_bytes_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..30)) {
            let mut f = Framer::default();
            for chunk in chunks { let _ = f.push(&chunk); }
        }
    }
}
