//! What `run` tells the user: one readable line per notable change of a speaker, and a
//! periodic summary of who is playing. Pure text logic, kept apart from the orchestrator so
//! it can be unit-tested.

use crate::proximity::Timestamp;
use crate::speaker::{Event, State};

/// A short label for a speaker's current state.
pub fn label(
    state: State,
    paired: bool,
    connected: bool,
    blocked: bool,
    probing: bool,
    now: Timestamp,
) -> String {
    match state {
        State::Absent => "gone".into(),
        State::InRange if !paired => "not paired".into(),
        State::InRange if blocked => "waiting for a free slot".into(),
        State::InRange if connected => "connected, not playing".into(),
        State::InRange => "idle".into(),
        State::Pairing => "pairing".into(),
        State::Connecting if probing => "connecting (probe)".into(),
        State::Connecting => "connecting".into(),
        State::AwaitingSink { .. } => "waiting for audio".into(),
        State::Linked => "playing".into(),
        State::Backoff { until } => {
            format!("retrying in {}s", until.saturating_sub(now).as_secs() + 1)
        }
        State::Disconnecting { .. } => "releasing".into(),
    }
}

/// The line to print when a speaker moved from `before` to `after` because of `event`, if
/// the change is worth telling the user about.
pub fn describe(
    before: State,
    after: State,
    event: Event,
    probing: bool,
    now: Timestamp,
) -> Option<String> {
    use State::*;
    let same_kind = std::mem::discriminant(&before) == std::mem::discriminant(&after);
    let retry = || match after {
        Backoff { until } => format!(", retrying in {}s", until.saturating_sub(now).as_secs() + 1),
        _ => String::new(),
    };
    let text = match (event, before, after) {
        (Event::Gone, Absent, _) => return None,
        (Event::Gone, _, _) => "went out of range".to_string(),
        (Event::PairResult(true), ..) => "paired".into(),
        (Event::PairResult(false), ..) => format!("pairing failed{}", retry()),
        (Event::ConnectResult(false), ..) => format!("could not connect{}", retry()),
        (_, _, Pairing) if !same_kind => "pairing".into(),
        (_, _, Connecting) if !same_kind && probing => {
            "connecting (nothing heard from it, so trying)".into()
        }
        (_, _, Connecting) if !same_kind => "connecting".into(),
        (_, _, Linked) if !same_kind => "playing".into(),
        (Event::Want(false), _, Disconnecting { .. }) => "moved away, releasing".into(),
        (Event::Connected(false), Linked | AwaitingSink { .. }, InRange) => {
            "lost the connection (powered off or out of range)".into()
        }
        (_, Disconnecting { .. }, InRange) => "disconnected".into(),
        (Event::SinkRemoved, Linked, AwaitingSink { .. }) => {
            "audio output went away, waiting for it".into()
        }
        (_, AwaitingSink { .. }, Backoff { .. }) => {
            format!("audio output never appeared{}", retry())
        }
        _ => return None,
    };
    Some(text)
}

/// One speaker as listed in the periodic summary.
pub struct Item {
    pub name: String,
    pub playing: bool,
    pub label: String,
}

/// The periodic one-line summary.
pub fn status_line(items: &[Item], max_connected: usize) -> String {
    let playing: Vec<&str> = items
        .iter()
        .filter(|i| i.playing)
        .map(|i| i.name.as_str())
        .collect();
    let others: Vec<String> = items
        .iter()
        .filter(|i| !i.playing && i.label != "gone")
        .map(|i| format!("{} ({})", i.name, i.label))
        .collect();
    let mut out = if playing.is_empty() {
        "status: not playing on any speaker".to_string()
    } else {
        format!(
            "status: playing on {} of {max_connected}: {}",
            playing.len(),
            playing.join(", ")
        )
    };
    if !others.is_empty() {
        out.push_str(&format!("; others: {}", others.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn t(s: u64) -> Timestamp {
        Duration::from_secs(s)
    }

    fn lines(before: State, after: State, event: Event) -> Option<String> {
        describe(before, after, event, false, t(100))
    }

    #[test]
    fn connection_lifecycle_reads_naturally() {
        use State::*;
        assert_eq!(
            lines(InRange, Connecting, Event::Want(true)).unwrap(),
            "connecting"
        );
        assert_eq!(
            lines(Connecting, Linked, Event::ConnectResult(true)).unwrap(),
            "playing"
        );
        assert_eq!(
            lines(
                Linked,
                Disconnecting {
                    deadline: t(1),
                    disconnect_sent: false
                },
                Event::Want(false)
            )
            .unwrap(),
            "moved away, releasing"
        );
        assert_eq!(
            lines(
                Disconnecting {
                    deadline: t(1),
                    disconnect_sent: true
                },
                InRange,
                Event::DisconnectResult(true)
            )
            .unwrap(),
            "disconnected"
        );
    }

    #[test]
    fn failures_say_when_they_retry() {
        use State::*;
        let text = lines(
            Connecting,
            Backoff { until: t(108) },
            Event::ConnectResult(false),
        )
        .unwrap();
        assert_eq!(text, "could not connect, retrying in 9s");
        let text = lines(Pairing, Backoff { until: t(102) }, Event::PairResult(false)).unwrap();
        assert_eq!(text, "pairing failed, retrying in 3s");
    }

    #[test]
    fn external_loss_is_not_called_a_release() {
        use State::*;
        assert!(
            lines(Linked, InRange, Event::Connected(false))
                .unwrap()
                .starts_with("lost the connection")
        );
        assert!(
            lines(
                AwaitingSink { deadline: t(1) },
                InRange,
                Event::Connected(false)
            )
            .unwrap()
            .starts_with("lost")
        );
    }

    #[test]
    fn probes_and_pairing_are_named() {
        use State::*;
        assert!(
            describe(InRange, Connecting, Event::Want(true), true, t(0))
                .unwrap()
                .contains("trying")
        );
        assert_eq!(
            lines(InRange, Pairing, Event::Want(true)).unwrap(),
            "pairing"
        );
        assert_eq!(
            lines(Pairing, InRange, Event::PairResult(true)).unwrap(),
            "paired"
        );
    }

    #[test]
    fn quiet_transitions_stay_quiet() {
        use State::*;
        assert!(lines(Absent, InRange, Event::Seen { paired: true }).is_none());
        assert!(lines(Linked, Linked, Event::Tick).is_none());
        assert!(lines(Absent, Absent, Event::Gone).is_none());
    }

    #[test]
    fn summary_lists_who_is_playing_and_who_is_not() {
        let item = |n: &str, p: bool, l: &str| Item {
            name: n.into(),
            playing: p,
            label: l.into(),
        };
        let text = status_line(
            &[
                item("JBL GO 2", true, "playing"),
                item("XKL-Q5", true, "playing"),
                item("Shokz", false, "idle"),
                item("Old", false, "gone"),
            ],
            3,
        );
        assert_eq!(
            text,
            "status: playing on 2 of 3: JBL GO 2, XKL-Q5; others: Shokz (idle)"
        );
        assert_eq!(status_line(&[], 3), "status: not playing on any speaker");
    }
}
