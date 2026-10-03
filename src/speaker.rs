//! Per-speaker state machine (DESIGN.md, "Per-Speaker State Machine"). Pure logic: it takes
//! events and a timestamp and returns the actions the orchestrator must perform. It never
//! does I/O and never reads a clock, so every transition is unit-testable.
//!
//! Proximity is *not* decided here. The orchestrator runs the proximity filter and the
//! `max_connected` admission and tells the machine whether it [`Event::Want`]s the
//! speaker; the machine turns that into connect/link/unlink/disconnect steps and copes
//! with failures and external changes.

// Wired up by the orchestrator.
#![allow(dead_code)]

use std::time::Duration;

use crate::proximity::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not seen (or gone). Nothing is held.
    Absent,
    /// Seen, idle. Paired or not; connected only if it connected on its own.
    InRange,
    /// Pairing in progress (`Pair` issued).
    Pairing,
    /// `Connect` issued.
    Connecting,
    /// Connected; waiting for the A2DP sink node.
    AwaitingSink { deadline: Timestamp },
    /// Sink present and our stream is linked to it.
    Linked,
    /// A step failed; retry after `until`.
    Backoff { until: Timestamp },
    /// Unlinking, then disconnecting.
    Disconnecting {
        deadline: Timestamp,
        /// The unlink is done (or none was needed) and `Disconnect` was issued.
        disconnect_sent: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A snapshot of the device was seen (any property change).
    Seen {
        paired: bool,
    },
    /// BlueZ removed the device, or it was not seen for a long time.
    Gone,
    /// `Device1.Connected` changed (or was read).
    Connected(bool),
    /// The A2DP sink node for this speaker appeared / disappeared.
    SinkAppeared,
    SinkRemoved,
    /// The audio engine confirms our link was removed.
    Unlinked,
    /// The orchestrator wants this speaker playing (`true`) or released (`false`).
    Want(bool),
    PairResult(bool),
    ConnectResult(bool),
    DisconnectResult(bool),
    /// Time passed; checks deadlines.
    Tick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pair,
    Connect,
    Disconnect,
    /// Set the volume (ramped), then link our stream to the sink.
    Link,
    Unlink,
    /// The link ended for a reason outside our control (speaker powered off, user
    /// disconnected it): the orchestrator starts the proximity cool-down so it is not
    /// immediately reconnected.
    ExternalDrop,
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub sink_timeout: Duration,
    pub disconnect_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            sink_timeout: Duration::from_secs(10),
            disconnect_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Machine {
    state: State,
    want: bool,
    paired: bool,
    connected: bool,
    sink: bool,
    /// Consecutive failures since the last time we reached `Linked`.
    failures: u32,
    auto_pair: bool,
    timing: Timing,
}

impl Machine {
    pub fn new(auto_pair: bool, timing: Timing) -> Self {
        Self {
            state: State::Absent,
            want: false,
            paired: false,
            connected: false,
            sink: false,
            failures: 0,
            auto_pair,
            timing,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// True while the machine holds or is acquiring the speaker.
    pub fn is_engaged(&self) -> bool {
        !matches!(
            self.state,
            State::Absent | State::InRange | State::Backoff { .. }
        )
    }

    pub fn handle(&mut self, event: Event, now: Timestamp) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::Seen { paired } => {
                self.paired = paired;
                if self.state == State::Absent {
                    self.state = State::InRange;
                    self.evaluate(now, &mut out);
                }
            }
            Event::Gone => self.gone(&mut out),
            Event::Want(want) => self.want_changed(want, now, &mut out),
            Event::Connected(c) => self.connected_changed(c, now, &mut out),
            Event::SinkAppeared => {
                self.sink = true;
                if matches!(self.state, State::AwaitingSink { .. }) {
                    self.acquired(&mut out);
                }
            }
            Event::SinkRemoved => {
                self.sink = false;
                if self.state == State::Linked {
                    // Typically a profile switch or a dropping link: drop our wish and
                    // wait for the sink to come back.
                    out.push(Action::Unlink);
                    self.state = State::AwaitingSink {
                        deadline: now + self.timing.sink_timeout,
                    };
                }
            }
            Event::Unlinked => {
                if let State::Disconnecting {
                    deadline,
                    disconnect_sent: false,
                } = self.state
                {
                    self.state = State::Disconnecting {
                        deadline,
                        disconnect_sent: true,
                    };
                    out.push(Action::Disconnect);
                }
            }
            Event::PairResult(ok) => {
                if self.state == State::Pairing {
                    if ok {
                        self.paired = true;
                        self.state = State::InRange;
                        self.evaluate(now, &mut out);
                    } else {
                        self.fail(now);
                    }
                }
            }
            Event::ConnectResult(ok) => {
                if self.state == State::Connecting {
                    if ok {
                        self.connected = true;
                        self.after_connect(now, &mut out);
                    } else {
                        self.fail(now);
                    }
                }
            }
            Event::DisconnectResult(ok) => {
                if matches!(self.state, State::Disconnecting { .. }) {
                    if ok || !self.connected {
                        self.connected = false;
                        self.state = State::InRange;
                    } else {
                        self.fail(now);
                    }
                }
            }
            Event::Tick => self.tick(now, &mut out),
        }
        out
    }

    /// In `InRange` (just arrived there, or a retry became due): decide what to do next.
    fn evaluate(&mut self, now: Timestamp, out: &mut Vec<Action>) {
        if self.state != State::InRange || !self.want {
            return;
        }
        if self.connected {
            self.after_connect(now, out);
        } else if !self.paired {
            if self.auto_pair {
                self.state = State::Pairing;
                out.push(Action::Pair);
            }
            // Without auto_pair the speaker stays InRange and is only reported.
        } else {
            self.state = State::Connecting;
            out.push(Action::Connect);
        }
    }

    /// We are connected: link now if the sink exists, else wait for it.
    fn after_connect(&mut self, now: Timestamp, out: &mut Vec<Action>) {
        if !self.want {
            self.begin_disconnect(now, false, out);
        } else if self.sink {
            self.acquired(out);
        } else {
            self.state = State::AwaitingSink {
                deadline: now + self.timing.sink_timeout,
            };
        }
    }

    fn acquired(&mut self, out: &mut Vec<Action>) {
        self.state = State::Linked;
        self.failures = 0;
        out.push(Action::Link);
    }

    fn begin_disconnect(&mut self, now: Timestamp, was_linked: bool, out: &mut Vec<Action>) {
        let deadline = now + self.timing.disconnect_timeout;
        if was_linked {
            out.push(Action::Unlink);
            self.state = State::Disconnecting {
                deadline,
                disconnect_sent: false,
            };
        } else {
            out.push(Action::Disconnect);
            self.state = State::Disconnecting {
                deadline,
                disconnect_sent: true,
            };
        }
    }

    fn fail(&mut self, now: Timestamp) {
        self.failures += 1;
        let factor = 1u32 << (self.failures - 1).min(16);
        let delay = (self.timing.backoff_min * factor).min(self.timing.backoff_max);
        self.state = State::Backoff { until: now + delay };
    }

    fn gone(&mut self, out: &mut Vec<Action>) {
        if matches!(self.state, State::Linked | State::Disconnecting { .. }) {
            out.push(Action::Unlink);
        }
        if self.state != State::Absent {
            out.push(Action::ExternalDrop);
        }
        self.state = State::Absent;
        self.want = false;
        self.connected = false;
        self.sink = false;
        self.failures = 0;
    }

    fn want_changed(&mut self, want: bool, now: Timestamp, out: &mut Vec<Action>) {
        if self.want == want {
            return;
        }
        self.want = want;
        match (want, self.state) {
            (true, State::InRange) => self.evaluate(now, out),
            (false, State::Linked) => self.begin_disconnect(now, true, out),
            (false, State::AwaitingSink { .. }) => self.begin_disconnect(now, false, out),
            (false, State::InRange) if self.connected => self.begin_disconnect(now, false, out),
            // Pairing and Connecting cannot be cancelled; the result is handled when it
            // arrives. Backoff re-evaluates when it expires.
            _ => {}
        }
    }

    fn connected_changed(&mut self, connected: bool, now: Timestamp, out: &mut Vec<Action>) {
        let was = self.connected;
        self.connected = connected;
        if connected == was {
            return;
        }
        match (connected, self.state) {
            (false, State::Linked | State::AwaitingSink { .. }) => {
                if self.state == State::Linked {
                    out.push(Action::Unlink);
                }
                out.push(Action::ExternalDrop);
                self.want = false;
                self.sink = false;
                self.state = State::InRange;
            }
            (false, State::Disconnecting { .. }) => {
                self.sink = false;
                self.state = State::InRange;
            }
            (false, _) => self.sink = false,
            // It reconnected on its own, or our connect succeeded: continue if we want it.
            (true, State::Backoff { .. }) => {
                self.state = State::InRange;
                self.evaluate(now, out);
            }
            (true, State::InRange) => self.evaluate(now, out),
            (true, _) => {}
        }
    }

    fn tick(&mut self, now: Timestamp, out: &mut Vec<Action>) {
        match self.state {
            State::AwaitingSink { deadline } if now >= deadline => {
                self.fail(now);
                out.push(Action::Disconnect);
            }
            State::Backoff { until } if now >= until => {
                self.state = State::InRange;
                self.evaluate(now, out);
            }
            State::Disconnecting {
                deadline,
                disconnect_sent,
            } if now >= deadline => {
                if disconnect_sent {
                    // Give up waiting; if it is still connected, the next Connected(false)
                    // or a retry sorts it out.
                    self.state = State::InRange;
                } else {
                    // The unlink confirmation never came; disconnect anyway.
                    self.state = State::Disconnecting {
                        deadline: now + self.timing.disconnect_timeout,
                        disconnect_sent: true,
                    };
                    out.push(Action::Disconnect);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Event::*;
    use super::*;

    fn t(s: u64) -> Timestamp {
        Duration::from_secs(s)
    }

    fn machine() -> Machine {
        Machine::new(true, Timing::default())
    }

    /// Run events at consecutive seconds starting at `t0`; return the final actions.
    fn run(m: &mut Machine, t0: u64, events: &[Event]) -> Vec<Vec<Action>> {
        events
            .iter()
            .enumerate()
            .map(|(i, e)| m.handle(*e, t(t0 + i as u64)))
            .collect()
    }

    fn in_range(paired: bool) -> Machine {
        let mut m = machine();
        m.handle(Seen { paired }, t(0));
        assert_eq!(m.state(), State::InRange);
        m
    }

    fn linked() -> Machine {
        let mut m = in_range(true);
        m.handle(Want(true), t(1));
        m.handle(ConnectResult(true), t(2));
        m.handle(SinkAppeared, t(3));
        assert_eq!(m.state(), State::Linked);
        m
    }

    #[test]
    fn seen_moves_absent_to_in_range_without_actions() {
        let mut m = machine();
        assert!(m.handle(Seen { paired: true }, t(0)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn happy_path_paired_speaker() {
        let mut m = in_range(true);
        assert_eq!(m.handle(Want(true), t(1)), [Connect]);
        assert_eq!(m.state(), State::Connecting);
        assert!(m.handle(ConnectResult(true), t(2)).is_empty());
        assert!(matches!(m.state(), State::AwaitingSink { .. }));
        assert_eq!(m.handle(SinkAppeared, t(3)), [Link]);
        assert_eq!(m.state(), State::Linked);
    }

    #[test]
    fn sink_already_present_links_on_connect() {
        let mut m = in_range(true);
        m.handle(SinkAppeared, t(0));
        m.handle(Want(true), t(1));
        assert_eq!(m.handle(ConnectResult(true), t(2)), [Link]);
    }

    #[test]
    fn connected_event_alone_also_advances_connecting() {
        let mut m = in_range(true);
        m.handle(Want(true), t(1));
        m.handle(Connected(true), t(2));
        // The connect call has not returned yet; wait for its result.
        assert_eq!(m.state(), State::Connecting);
        m.handle(ConnectResult(true), t(3));
        assert!(matches!(m.state(), State::AwaitingSink { .. }));
    }

    #[test]
    fn unpaired_speaker_pairs_first() {
        let mut m = in_range(false);
        assert_eq!(m.handle(Want(true), t(1)), [Pair]);
        assert_eq!(m.state(), State::Pairing);
        assert_eq!(m.handle(PairResult(true), t(2)), [Connect]);
        assert_eq!(m.state(), State::Connecting);
    }

    #[test]
    fn unpaired_without_auto_pair_stays_in_range() {
        let mut m = Machine::new(false, Timing::default());
        m.handle(Seen { paired: false }, t(0));
        assert!(m.handle(Want(true), t(1)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn pair_failure_backs_off_then_retries_pairing() {
        let mut m = in_range(false);
        m.handle(Want(true), t(1));
        assert!(m.handle(PairResult(false), t(2)).is_empty());
        assert_eq!(m.state(), State::Backoff { until: t(4) });
        assert!(m.handle(Tick, t(3)).is_empty());
        assert_eq!(m.handle(Tick, t(4)), [Pair]);
    }

    #[test]
    fn connect_failure_backs_off_exponentially_up_to_the_cap() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        let mut now = 0;
        let mut delays = Vec::new();
        for _ in 0..8 {
            m.handle(ConnectResult(false), t(now));
            let State::Backoff { until } = m.state() else {
                panic!("not in backoff: {:?}", m.state());
            };
            delays.push((until - t(now)).as_secs());
            now = until.as_secs();
            assert_eq!(m.handle(Tick, t(now)), [Connect]);
        }
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn reaching_linked_resets_the_failure_count() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(false), t(1));
        m.handle(Tick, t(3));
        m.handle(ConnectResult(true), t(4));
        m.handle(SinkAppeared, t(5));
        assert_eq!(m.state(), State::Linked);
        // Drop and fail again: the delay starts from the minimum.
        m.handle(Want(false), t(6));
        m.handle(Unlinked, t(7));
        m.handle(DisconnectResult(true), t(8));
        m.handle(Want(true), t(9));
        m.handle(ConnectResult(false), t(10));
        assert_eq!(m.state(), State::Backoff { until: t(12) });
    }

    #[test]
    fn awaiting_sink_times_out_to_backoff_and_disconnects() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(true), t(1));
        assert!(m.handle(Tick, t(10)).is_empty());
        assert_eq!(m.handle(Tick, t(11)), [Disconnect]);
        assert!(matches!(m.state(), State::Backoff { .. }));
    }

    #[test]
    fn backoff_retry_when_still_wanted_but_not_when_released() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(false), t(1));
        m.handle(Want(false), t(2));
        assert!(m.handle(Tick, t(3)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn release_while_linked_unlinks_then_disconnects() {
        let mut m = linked();
        assert_eq!(m.handle(Want(false), t(10)), [Unlink]);
        assert!(matches!(
            m.state(),
            State::Disconnecting {
                disconnect_sent: false,
                ..
            }
        ));
        assert_eq!(m.handle(Unlinked, t(11)), [Disconnect]);
        assert!(m.handle(DisconnectResult(true), t(12)).is_empty());
        assert_eq!(m.state(), State::InRange);
        assert!(!m.is_connected());
    }

    #[test]
    fn release_while_awaiting_sink_disconnects_directly() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(true), t(1));
        assert_eq!(m.handle(Want(false), t(2)), [Disconnect]);
    }

    #[test]
    fn release_while_connecting_disconnects_once_connected() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        assert!(m.handle(Want(false), t(1)).is_empty());
        assert_eq!(m.state(), State::Connecting);
        assert_eq!(m.handle(ConnectResult(true), t(2)), [Disconnect]);
    }

    #[test]
    fn release_while_pairing_pairs_but_does_not_connect() {
        let mut m = in_range(false);
        m.handle(Want(true), t(0));
        m.handle(Want(false), t(1));
        assert!(m.handle(PairResult(true), t(2)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn unlinked_timeout_disconnects_anyway() {
        let mut m = linked();
        m.handle(Want(false), t(10));
        assert!(m.handle(Tick, t(19)).is_empty());
        assert_eq!(m.handle(Tick, t(20)), [Disconnect]);
    }

    #[test]
    fn disconnect_failure_backs_off() {
        let mut m = linked();
        m.handle(Want(false), t(10));
        m.handle(Unlinked, t(11));
        m.handle(DisconnectResult(false), t(12));
        assert!(matches!(m.state(), State::Backoff { .. }));
    }

    #[test]
    fn external_disconnect_while_linked_unlinks_and_cools_down() {
        let mut m = linked();
        assert_eq!(m.handle(Connected(false), t(10)), [Unlink, ExternalDrop]);
        assert_eq!(m.state(), State::InRange);
        // No longer wanted: it does not reconnect until the orchestrator asks again.
        assert!(m.handle(Tick, t(11)).is_empty());
        assert_eq!(m.handle(Want(true), t(40)), [Connect]);
    }

    #[test]
    fn external_disconnect_while_awaiting_sink() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(true), t(1));
        assert_eq!(m.handle(Connected(false), t(2)), [ExternalDrop]);
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn disconnected_event_completes_a_disconnect_we_started() {
        let mut m = linked();
        m.handle(Want(false), t(10));
        m.handle(Unlinked, t(11));
        assert!(m.handle(Connected(false), t(12)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn sink_vanishing_while_linked_waits_for_it_to_return() {
        let mut m = linked();
        assert_eq!(m.handle(SinkRemoved, t(10)), [Unlink]);
        assert!(matches!(m.state(), State::AwaitingSink { .. }));
        assert_eq!(m.handle(SinkAppeared, t(12)), [Link]);
        assert_eq!(m.state(), State::Linked);
    }

    #[test]
    fn sink_that_never_returns_times_out() {
        let mut m = linked();
        m.handle(SinkRemoved, t(10));
        assert_eq!(m.handle(Tick, t(20)), [Disconnect]);
        assert!(matches!(m.state(), State::Backoff { .. }));
    }

    #[test]
    fn speaker_that_reconnects_by_itself_is_picked_up_when_wanted() {
        let mut m = in_range(true);
        assert!(m.handle(Connected(true), t(1)).is_empty());
        assert_eq!(m.state(), State::InRange);
        assert!(m.handle(SinkAppeared, t(2)).is_empty());
        assert_eq!(m.handle(Want(true), t(3)), [Link]);
        assert_eq!(m.state(), State::Linked);
    }

    #[test]
    fn self_connected_speaker_that_is_released_gets_disconnected() {
        let mut m = in_range(true);
        m.handle(Connected(true), t(1));
        m.handle(Want(true), t(2));
        m.handle(Want(false), t(3));
        assert_eq!(
            m.state(),
            State::Disconnecting {
                deadline: t(13),
                disconnect_sent: true,
            }
        );
    }

    #[test]
    fn connected_during_backoff_resumes_immediately() {
        let mut m = in_range(true);
        m.handle(Want(true), t(0));
        m.handle(ConnectResult(false), t(1));
        m.handle(SinkAppeared, t(1));
        assert_eq!(m.handle(Connected(true), t(2)), [Link]);
    }

    #[test]
    fn gone_while_linked_unlinks_and_resets() {
        let mut m = linked();
        assert_eq!(m.handle(Gone, t(10)), [Unlink, ExternalDrop]);
        assert_eq!(m.state(), State::Absent);
        assert!(!m.is_connected());
        // Seen again later starts from scratch.
        m.handle(Seen { paired: true }, t(20));
        assert_eq!(m.state(), State::InRange);
        assert_eq!(m.handle(Want(true), t(21)), [Connect]);
    }

    #[test]
    fn gone_when_idle_is_silent() {
        let mut m = machine();
        assert!(m.handle(Gone, t(0)).is_empty());
        let mut m = in_range(true);
        assert_eq!(m.handle(Gone, t(1)), [ExternalDrop]);
    }

    #[test]
    fn stale_results_in_other_states_are_ignored() {
        let mut m = in_range(true);
        assert!(m.handle(ConnectResult(true), t(1)).is_empty());
        assert!(m.handle(PairResult(true), t(1)).is_empty());
        assert!(m.handle(DisconnectResult(true), t(1)).is_empty());
        assert!(m.handle(Unlinked, t(1)).is_empty());
        assert_eq!(m.state(), State::InRange);
    }

    #[test]
    fn repeated_want_is_idempotent() {
        let mut m = in_range(true);
        assert_eq!(m.handle(Want(true), t(1)), [Connect]);
        assert!(m.handle(Want(true), t(2)).is_empty());
    }

    #[test]
    fn full_walk_up_and_away() {
        let mut m = in_range(true);
        let out = run(
            &mut m,
            1,
            &[
                Want(true),
                ConnectResult(true),
                SinkAppeared,
                Want(false),
                Unlinked,
                DisconnectResult(true),
                Want(true),
            ],
        );
        assert_eq!(
            out,
            [
                vec![Connect],
                vec![],
                vec![Link],
                vec![Unlink],
                vec![Disconnect],
                vec![],
                vec![Connect],
            ]
        );
    }
}
