use super::*;

pub(super) fn terminal_event(
    lines: &[String],
    previously_answered: bool,
) -> Option<(AnswerClassification, EndReason)> {
    if lines
        .iter()
        .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::Busy)))
    {
        return Some((AnswerClassification::NotAnswered, EndReason::Busy));
    }
    if lines
        .iter()
        .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::NoAnswer)))
    {
        return Some((AnswerClassification::NotAnswered, EndReason::NoAnswer));
    }
    let answered = previously_answered
        || lines
            .iter()
            .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::VoiceBegin)));
    let ended = lines.iter().any(|line| {
        matches!(
            parse_urc(line),
            Some(crate::call::CallUrc::VoiceEnd | crate::call::CallUrc::NoCarrier)
        )
    });
    ended.then_some(if answered {
        (AnswerClassification::Answered, EndReason::RemoteHangUp)
    } else {
        (AnswerClassification::Unknown, EndReason::CallError)
    })
}

pub(super) fn classification_label(value: AnswerClassification) -> &'static str {
    match value {
        AnswerClassification::Unknown => "unknown",
        AnswerClassification::Answered => "answered",
        AnswerClassification::NotAnswered => "not-answered",
    }
}

pub(super) fn end_reason_label(value: EndReason) -> &'static str {
    match value {
        EndReason::None => "none",
        EndReason::LocalHangUp => "local-hang-up",
        EndReason::RemoteHangUp => "remote-hang-up",
        EndReason::Busy => "busy",
        EndReason::NoAnswer => "no-answer",
        EndReason::Unreachable => "unreachable",
        EndReason::NetworkError => "network-error",
        EndReason::SignalingTimeout => "signaling-timeout",
        EndReason::ModemLost => "modem-lost",
        EndReason::CallError => "call-error",
    }
}
