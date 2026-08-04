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
    pending_sms_urcs: Vec<String>,
    complete_sms_urcs: Vec<Vec<String>>,
    awaiting_cds_pdu: Option<String>,
}

impl Dispatcher {
    /// Keeps one framing buffer for the lifetime of the port and separates
    /// unsolicited call notifications from the active command response.
    pub fn push(&mut self, bytes: &[u8], command: Option<&str>) -> (Vec<Frame>, Vec<String>) {
        let mut response = Vec::new();
        let mut urcs = Vec::new();
        for frame in self.framer.push(bytes) {
            match frame {
                Frame::Line(line) if self.awaiting_cds_pdu.is_some() => {
                    let header = self.awaiting_cds_pdu.take().expect("checked above");
                    self.pending_sms_urcs.push(line.clone());
                    self.complete_sms_urcs.push(vec![header, line.clone()]);
                    urcs.push(line);
                }
                Frame::Line(line) if line.starts_with("+CDS:") => {
                    self.pending_sms_urcs.push(line.clone());
                    if line
                        .strip_prefix("+CDS:")
                        .is_some_and(|value| value.trim().parse::<usize>().is_ok())
                    {
                        self.awaiting_cds_pdu = Some(line.clone());
                    } else {
                        self.complete_sms_urcs.push(vec![line.clone()]);
                    }
                    urcs.push(line);
                }
                Frame::Line(line) if line.starts_with("+CDSI:") || line.starts_with("+CMTI:") => {
                    self.pending_sms_urcs.push(line.clone());
                    self.complete_sms_urcs.push(vec![line.clone()]);
                    urcs.push(line);
                }
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

    pub fn take_sms_urcs(&mut self) -> Vec<String> {
        mem::take(&mut self.pending_sms_urcs)
    }

    pub fn take_complete_sms_urcs(&mut self) -> Vec<Vec<String>> {
        mem::take(&mut self.complete_sms_urcs)
    }

    pub fn reset(&mut self) {
        self.framer.reset();
        self.pending_urcs.clear();
        self.pending_sms_urcs.clear();
        self.complete_sms_urcs.clear();
        self.awaiting_cds_pdu = None;
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
    for protected in ["CMGF", "CPMS", "CSMP", "CNMI"] {
        if let Some(position) = upper.find(protected) {
            let suffix = upper[position + protected.len()..].trim();
            if !matches!(suffix, "?" | "=?") {
                return Err(crate::ModemError::Validation(
                    "SMS mode and delivery-tracking configuration is daemon-owned; query forms remain available".into(),
                ));
            }
        }
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
        assert!(validate_console("AT+CNMI=2,1,0,2,0", false).is_err());
        assert!(validate_console("AT+CSMP=49,167,0,0", false).is_err());
        assert_eq!(validate_console("AT+CNMI?", false).unwrap(), "AT+CNMI?");
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
    #[test]
    fn dispatcher_keeps_split_sms_reports_out_of_responses() {
        let mut d = Dispatcher::default();
        let (first, _) = d.push(b"+CDSI: \"SM\",7\r\n+CDS: 24\r\n0011", Some("AT+CSQ"));
        assert!(first.is_empty());
        let (second, _) = d.push(b"2233\r\n+CSQ: 10,99\r\nOK\r\n", Some("AT+CSQ"));
        assert_eq!(
            second,
            vec![Frame::Line("+CSQ: 10,99".into()), Frame::Line("OK".into())]
        );
        assert_eq!(
            d.take_sms_urcs(),
            vec!["+CDSI: \"SM\",7", "+CDS: 24", "00112233"]
        );
    }
    #[test]
    fn dispatcher_does_not_swallow_command_result_after_text_mode_cds() {
        let mut d = Dispatcher::default();
        let input = b"+CDS: 2,42,\"+66812345678\",145,\"26/08/04,12:00:00+00\",\"26/08/04,12:01:00+00\",0\r\n+CMGS: 42\r\nOK\r\n";
        let (response, _) = d.push(input, Some("AT+CMGS=\"+66812345678\""));
        assert_eq!(
            response,
            vec![Frame::Line("+CMGS: 42".into()), Frame::Line("OK".into())]
        );
        assert_eq!(
            d.take_complete_sms_urcs(),
            vec![vec![
                String::from_utf8_lossy(input)
                    .lines()
                    .next()
                    .unwrap()
                    .trim_end_matches('\r')
                    .to_owned()
            ]]
        );
    }

    #[test]
    fn dispatcher_emits_fragmented_pdu_report_only_when_complete() {
        let mut d = Dispatcher::default();
        d.push(b"+CDS: 24\r\n0011", None);
        assert!(d.take_complete_sms_urcs().is_empty());
        d.push(b"2233\r\n", None);
        assert_eq!(
            d.take_complete_sms_urcs(),
            vec![vec![String::from("+CDS: 24"), String::from("00112233")]]
        );
    }
    proptest::proptest! {
        #[test] fn arbitrary_bytes_never_panic(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..30)) {
            let mut f = Framer::default();
            for chunk in chunks { let _ = f.push(&chunk); }
        }
    }
}
