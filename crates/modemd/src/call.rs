use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnswerClassification {
    #[default]
    Unknown,
    Answered,
    NotAnswered,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EndReason {
    #[default]
    None,
    LocalHangUp,
    RemoteHangUp,
    Busy,
    NoAnswer,
    Unreachable,
    NetworkError,
    SignalingTimeout,
    ModemLost,
    CallError,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallProgress {
    #[default]
    Dialing,
    Ringing,
    Active,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallUrc {
    Clcc { direction: u8, state: u8 },
    VoiceBegin,
    VoiceEnd,
    Busy,
    NoAnswer,
    NoCarrier,
}

pub fn parse_urc(line: &str) -> Option<CallUrc> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("+CLCC:") {
        let fields: Vec<_> = rest.split(',').map(str::trim).collect();
        return Some(CallUrc::Clcc {
            direction: fields.get(1)?.parse().ok()?,
            state: fields.get(2)?.parse().ok()?,
        });
    }
    match line {
        "VOICE CALL: BEGIN" | "VOICE CALL:BEGIN" => Some(CallUrc::VoiceBegin),
        value if value.starts_with("VOICE CALL: END") || value.starts_with("VOICE CALL:END") => {
            Some(CallUrc::VoiceEnd)
        }
        "BUSY" => Some(CallUrc::Busy),
        "NO ANSWER" => Some(CallUrc::NoAnswer),
        "NO CARRIER" => Some(CallUrc::NoCarrier),
        _ => None,
    }
}

#[derive(Debug)]
pub struct CallTracker {
    pub progress: CallProgress,
    pub classification: AnswerClassification,
    pub end_reason: EndReason,
    pub connected_at: Option<Instant>,
    pub ended_at: Option<Instant>,
    local_hang_up: bool,
}

impl Default for CallTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CallTracker {
    pub fn new() -> Self {
        Self {
            progress: CallProgress::Dialing,
            classification: AnswerClassification::Unknown,
            end_reason: EndReason::None,
            connected_at: None,
            ended_at: None,
            local_hang_up: false,
        }
    }

    pub fn local_hang_up_requested(&mut self) {
        self.local_hang_up = true;
    }

    /// Applies one modem event. Events received after a terminal outcome are stale
    /// and cannot modify this call.
    pub fn observe(&mut self, event: CallUrc, now: Instant) {
        if self.ended_at.is_some() {
            return;
        }
        match event {
            CallUrc::Clcc {
                direction: 0,
                state: 2,
            } => self.progress = CallProgress::Dialing,
            CallUrc::Clcc {
                direction: 0,
                state: 3,
            } => self.progress = CallProgress::Ringing,
            CallUrc::Clcc {
                direction: 0,
                state: 0 | 4 | 5,
            } => self.progress = CallProgress::Active,
            CallUrc::VoiceBegin => {
                self.progress = CallProgress::Active;
                self.classification = AnswerClassification::Answered;
                self.connected_at.get_or_insert(now);
            }
            CallUrc::Busy => self.finish(AnswerClassification::NotAnswered, EndReason::Busy, now),
            CallUrc::NoAnswer => {
                self.finish(AnswerClassification::NotAnswered, EndReason::NoAnswer, now)
            }
            CallUrc::VoiceEnd => {
                let reason = if self.local_hang_up {
                    EndReason::LocalHangUp
                } else if self.classification == AnswerClassification::Answered {
                    EndReason::RemoteHangUp
                } else {
                    EndReason::NoAnswer
                };
                let classification = if self.classification == AnswerClassification::Answered {
                    AnswerClassification::Answered
                } else {
                    AnswerClassification::NotAnswered
                };
                self.finish(classification, reason, now);
            }
            CallUrc::NoCarrier => {
                let classification = self.classification;
                let reason = if self.local_hang_up {
                    EndReason::LocalHangUp
                } else {
                    EndReason::CallError
                };
                self.finish(classification, reason, now);
            }
            _ => {}
        }
    }

    pub fn signaling_timeout(&mut self, now: Instant) {
        if self.ended_at.is_none() {
            self.finish(
                AnswerClassification::Unknown,
                EndReason::SignalingTimeout,
                now,
            );
        }
    }

    pub fn apply_ceer(&mut self, cause: &str) {
        if self.local_hang_up
            || matches!(
                self.end_reason,
                EndReason::LocalHangUp | EndReason::Busy | EndReason::NoAnswer
            )
        {
            return;
        }
        self.end_reason =
            classify_ceer(cause, self.classification == AnswerClassification::Answered);
        if self.classification == AnswerClassification::Unknown {
            self.classification = AnswerClassification::NotAnswered;
        }
    }

    fn finish(&mut self, classification: AnswerClassification, reason: EndReason, now: Instant) {
        self.progress = CallProgress::Ended;
        self.classification = classification;
        self.end_reason = reason;
        self.ended_at = Some(now);
    }
}

pub fn classify_ceer(cause: &str, answered: bool) -> EndReason {
    let cause = cause.to_ascii_lowercase();
    if [
        "subscriber absent",
        "destination out of order",
        "destination unavailable",
        "unallocated",
        "no route to destination",
        "not reachable",
        "cannot connect",
    ]
    .iter()
    .any(|needle| cause.contains(needle))
    {
        EndReason::Unreachable
    } else if [
        "congestion",
        "temporary failure",
        "no circuit",
        "no channel",
        "network failure",
        "switching equipment congestion",
        "resource unavailable",
    ]
    .iter()
    .any(|needle| cause.contains(needle))
    {
        EndReason::NetworkError
    } else if cause.contains("user busy") {
        EndReason::Busy
    } else if cause.contains("no answer") {
        EndReason::NoAnswer
    } else if cause.contains("normal") || cause.contains("unspecified") {
        if answered {
            EndReason::RemoteHangUp
        } else {
            EndReason::NoAnswer
        }
    } else {
        EndReason::CallError
    }
}

/// Removes controls and bounds modem-provided details before displaying or storing them.
pub fn sanitize_cause(cause: &str) -> String {
    cause
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_then_end_is_answered_remote_hang_up() {
        let now = Instant::now();
        let mut call = CallTracker::new();
        call.observe(CallUrc::VoiceBegin, now);
        call.observe(CallUrc::VoiceEnd, now);
        assert_eq!(call.classification, AnswerClassification::Answered);
        assert_eq!(call.end_reason, EndReason::RemoteHangUp);
    }

    #[test]
    fn end_without_begin_is_not_answered() {
        let now = Instant::now();
        let mut call = CallTracker::new();
        call.observe(CallUrc::VoiceEnd, now);
        assert_eq!(call.classification, AnswerClassification::NotAnswered);
        assert_eq!(call.end_reason, EndReason::NoAnswer);
    }

    #[test]
    fn clcc_active_never_proves_answer() {
        let mut call = CallTracker::new();
        call.observe(
            CallUrc::Clcc {
                direction: 0,
                state: 0,
            },
            Instant::now(),
        );
        assert_eq!(call.progress, CallProgress::Active);
        assert_eq!(call.classification, AnswerClassification::Unknown);
    }

    #[test]
    fn ceer_categories_and_unknown_fallback_are_safe() {
        assert_eq!(
            classify_ceer("1 Subscriber absent", false),
            EndReason::Unreachable
        );
        assert_eq!(
            classify_ceer("34 No circuit/channel available", false),
            EndReason::NetworkError
        );
        assert_eq!(
            classify_ceer("16 Normal call clearing", false),
            EndReason::NoAnswer
        );
        assert_eq!(
            classify_ceer("16 Normal call clearing", true),
            EndReason::RemoteHangUp
        );
        assert_eq!(classify_ceer("vendor mystery", false), EndReason::CallError);
    }

    #[test]
    fn fragments_duplicate_and_late_events_do_not_change_completed_outcome() {
        let mut framer = crate::at::Framer::default();
        assert!(framer.push(b"VOICE CALL: BE").is_empty());
        let lines = framer.push(b"GIN\r\nVOICE CALL: END: 12\r\nBUSY\r\n");
        let mut call = CallTracker::new();
        for event in lines.into_iter().filter_map(|frame| match frame {
            crate::at::Frame::Line(line) => parse_urc(&line),
            _ => None,
        }) {
            call.observe(event, Instant::now());
        }
        assert_eq!(call.classification, AnswerClassification::Answered);
        assert_eq!(call.end_reason, EndReason::RemoteHangUp);
    }
}
