#[derive(Clone, Copy, Default)]
pub(super) enum CallScenario {
    #[default]
    Answered,
    NoAnswer,
    Busy,
    Unreachable,
    NetworkError,
    MissingTerminal,
}

#[derive(Default)]
pub(super) struct SimState {
    pub(super) legacy_scenario: CallScenario,
    pub(super) current_audio: Option<serde_json::Value>,
    pub(super) audio: Vec<serde_json::Value>,
    pub(super) audio_sequence: u64,
    pub(super) calls: Vec<serde_json::Value>,
    pub(super) call_scenario: String,
    pub(super) polls: u32,
    pub(super) settings: Option<serde_json::Value>,
    pub(super) hang_attempts: u32,
    pub(super) sms: Vec<serde_json::Value>,
    pub(super) sms_polls: u32,
}

pub(super) fn advance_calls(state: &mut SimState) {
    state.polls += 1;
    if let Some(call) = state.calls.first_mut() {
        let (status, answer, reason, error) = match (state.call_scenario.as_str(), state.polls) {
            ("no-answer", 3..) => ("ended", "not-answered", "no-answer", ""),
            ("busy", 2..) => ("ended", "not-answered", "busy", ""),
            ("playback-failure", 4..) => {
                ("failed", "answered", "call-error", "playback start failed")
            }
            ("missing-completion", 10..) => (
                "failed",
                "answered",
                "call-error",
                "playback completion timed out",
            ),
            ("early-remote-hang-up", 2..) => ("ended", "answered", "remote-hang-up", ""),
            ("manual-cancellation", 3..) => ("playing", "answered", "none", ""),
            ("stuck-release", _) if call["state"] == "hang-up-failed" => (
                "hang-up-failed",
                "answered",
                "none",
                "release confirmation timed out",
            ),
            ("delayed-answer", 1..=5) => ("waiting-for-answer", "unknown", "none", ""),
            (_, 1) => ("playback-delay", "answered", "none", ""),
            (_, poll) if (2..=4).contains(&poll) => ("playing", "answered", "none", ""),
            (_, poll) if poll >= 5 => ("ended", "answered", "local-hang-up", ""),
            _ => ("waiting-for-answer", "unknown", "none", ""),
        };
        call["state"] = status.into();
        call["answerClassification"] = answer.into();
        call["endReason"] = reason.into();
        call["error"] = error.into();
    }
}
