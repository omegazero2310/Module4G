use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnswerClassification {
    #[default]
    Unknown,
    InferredHuman,
    Forwarded,
    NoAnswer,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EndReason {
    #[default]
    None,
    LocalHangUp,
    RemoteOrNetworkHangUp,
    Busy,
    NetworkNoAnswer,
    AlertTimeout,
    ModemLost,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallUrc {
    Clcc { direction: u8, state: u8 },
    Forwarded,
    VoiceBegin,
    VoiceEnd,
    Busy,
    NoAnswer,
    NoCarrier,
}

pub fn parse_urc(line: &str) -> Option<CallUrc> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("+CSSI:") {
        return (rest.trim().split(',').next()?.trim() == "2").then_some(CallUrc::Forwarded);
    }
    if let Some(rest) = line.strip_prefix("+CLCC:") {
        let fields: Vec<_> = rest.split(',').map(str::trim).collect();
        return Some(CallUrc::Clcc {
            direction: fields.get(1)?.parse().ok()?,
            state: fields.get(2)?.parse().ok()?,
        });
    }
    match line {
        "VOICE CALL: BEGIN" | "VOICE CALL:BEGIN" => Some(CallUrc::VoiceBegin),
        "VOICE CALL: END" | "VOICE CALL:END" => Some(CallUrc::VoiceEnd),
        "BUSY" => Some(CallUrc::Busy),
        "NO ANSWER" => Some(CallUrc::NoAnswer),
        "NO CARRIER" => Some(CallUrc::NoCarrier),
        _ => None,
    }
}

#[derive(Debug)]
pub struct CallTracker {
    pub classification: AnswerClassification,
    pub end_reason: EndReason,
    pub forwarding_seen: bool,
    pub connected_at: Option<Instant>,
    pub ended_at: Option<Instant>,
    alerting_at: Option<Instant>,
    local_hang_up: bool,
    guard: Duration,
}

impl CallTracker {
    pub fn new(guard: Duration) -> Self {
        Self {
            classification: AnswerClassification::Unknown,
            end_reason: EndReason::None,
            forwarding_seen: false,
            connected_at: None,
            ended_at: None,
            alerting_at: None,
            local_hang_up: false,
            guard,
        }
    }
    pub fn local_hang_up_requested(&mut self) {
        self.local_hang_up = true;
    }
    /// Returns true when the daemon should issue AT+CHUP.
    pub fn observe(&mut self, event: CallUrc, now: Instant) -> bool {
        match event {
            CallUrc::Forwarded => {
                self.forwarding_seen = true;
                self.classification = AnswerClassification::Forwarded;
                true
            }
            CallUrc::Clcc {
                direction: 0,
                state: 3,
            } => {
                self.alerting_at.get_or_insert(now);
                false
            }
            CallUrc::Clcc {
                direction: 0,
                state: 0,
            }
            | CallUrc::VoiceBegin => {
                self.connected_at.get_or_insert(now);
                false
            }
            CallUrc::Busy => {
                self.end_reason = EndReason::Busy;
                self.ended_at = Some(now);
                false
            }
            CallUrc::NoAnswer => {
                self.classification = AnswerClassification::NoAnswer;
                self.end_reason = EndReason::NetworkNoAnswer;
                self.ended_at = Some(now);
                false
            }
            CallUrc::NoCarrier | CallUrc::VoiceEnd => {
                self.end_reason = if self.local_hang_up {
                    EndReason::LocalHangUp
                } else {
                    EndReason::RemoteOrNetworkHangUp
                };
                self.ended_at = Some(now);
                false
            }
            _ => false,
        }
    }
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.ended_at.is_some() || self.forwarding_seen {
            return false;
        }
        if self
            .alerting_at
            .is_some_and(|at| now.saturating_duration_since(at) >= self.guard)
        {
            self.classification = AnswerClassification::NoAnswer;
            self.end_reason = EndReason::AlertTimeout;
            return true;
        }
        if self
            .connected_at
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_millis(500))
        {
            self.classification = AnswerClassification::InferredHuman;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_fragment_reassembled_call_urcs() {
        let mut f = crate::at::Framer::default();
        assert!(f.push(b"+CSS").is_empty());
        let frames = f.push(b"I: 2\r\n+CLCC: 1,0,3,0,0\r\n");
        let urcs: Vec<_> = frames
            .into_iter()
            .filter_map(|x| match x {
                crate::at::Frame::Line(x) => parse_urc(&x),
                _ => None,
            })
            .collect();
        assert_eq!(
            urcs,
            [
                CallUrc::Forwarded,
                CallUrc::Clcc {
                    direction: 0,
                    state: 3
                }
            ]
        );
    }
    #[test]
    fn cssi_zero_and_one_are_ignored() {
        assert_eq!(parse_urc("+CSSI: 0"), None);
        assert_eq!(parse_urc("+CSSI: 1"), None);
    }
    #[test]
    fn alert_timeout_wins_and_late_forwarding_reclassifies() {
        let start = Instant::now();
        let mut c = CallTracker::new(Duration::from_secs(15));
        c.observe(
            CallUrc::Clcc {
                direction: 0,
                state: 3,
            },
            start,
        );
        c.observe(
            CallUrc::Clcc {
                direction: 0,
                state: 0,
            },
            start + Duration::from_secs(14),
        );
        assert!(c.tick(start + Duration::from_secs(15)));
        assert_eq!(c.classification, AnswerClassification::NoAnswer);
        assert!(c.observe(CallUrc::Forwarded, start + Duration::from_secs(16)));
        assert_eq!(c.classification, AnswerClassification::Forwarded);
    }
    #[test]
    fn answer_is_inferred_after_grace() {
        let start = Instant::now();
        let mut c = CallTracker::new(Duration::from_secs(15));
        c.observe(
            CallUrc::Clcc {
                direction: 0,
                state: 0,
            },
            start,
        );
        c.tick(start + Duration::from_millis(499));
        assert_eq!(c.classification, AnswerClassification::Unknown);
        c.tick(start + Duration::from_millis(500));
        assert_eq!(c.classification, AnswerClassification::InferredHuman);
    }
}
