//! Proximity filter for one speaker: time-based EMA, hysteresis with dwell times, a stale
//! sample timeout and a re-accept cool-down. Pure logic: the caller supplies every
//! timestamp, so it is deterministic and needs no I/O. See DESIGN.md, decision 2.
//!
//! The filter works on any scale where *higher means closer*: true dBm from discovery, or
//! the relative dB-below-golden-range of the mgmt socket. Each scale gets its own
//! [`Params`].

// Wired up by the Bluetooth manager (M4) and orchestrator (M6).
#![allow(dead_code)]

use std::time::Duration;

use crate::config::{MgmtProximityConfig, ProximityConfig};

/// Monotonic time since an arbitrary epoch chosen by the caller.
pub type Timestamp = Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Smoothed value at or above this counts as near...
    pub near: f64,
    /// ...for at least this long to become [`State::Near`].
    pub near_dwell: Duration,
    /// Smoothed value below this counts as far...
    pub far: f64,
    /// ...for at least this long to become [`State::Far`].
    pub far_dwell: Duration,
    /// EMA time constant.
    pub tau: Duration,
    /// No sample for this long counts as far.
    pub stale_after: Duration,
    /// After becoming far, refuse to become near again for this long.
    pub cooldown: Duration,
}

impl Params {
    /// Discovery RSSI (true dBm).
    pub fn dbm(c: &ProximityConfig) -> Self {
        Self {
            near: f64::from(c.connect_rssi),
            near_dwell: secs(c.connect_dwell_secs),
            far: f64::from(c.disconnect_rssi),
            far_dwell: secs(c.disconnect_dwell_secs),
            tau: secs(c.ema_tau_secs),
            stale_after: secs(c.stale_after_secs),
            cooldown: secs(c.cooldown_secs),
        }
    }

    /// Mgmt socket RSSI (dB below the golden receive range, 0 = ideal).
    pub fn mgmt(c: &ProximityConfig) -> Self {
        let m: &MgmtProximityConfig = &c.mgmt;
        Self {
            near: f64::from(m.connect_db),
            near_dwell: secs(m.connect_dwell_secs),
            far: f64::from(m.disconnect_db),
            far_dwell: secs(m.disconnect_dwell_secs),
            tau: secs(m.ema_tau_secs),
            ..Self::dbm(c)
        }
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Near,
    Far,
}

/// Why a transition happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The smoothed value stayed past the threshold for the dwell time.
    Dwell,
    /// No sample arrived within `stale_after`.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub to: State,
    pub reason: Reason,
}

#[derive(Debug, Clone)]
pub struct Tracker {
    params: Params,
    state: State,
    ema: Option<f64>,
    last_sample: Option<Timestamp>,
    /// Since when the smoothed value has been continuously past the threshold for leaving
    /// the current state.
    pending_since: Option<Timestamp>,
    /// Becoming near is refused before this time.
    cooldown_until: Option<Timestamp>,
}

impl Tracker {
    /// Start in `state`. Use [`State::Near`] for a speaker that connected by itself (it is
    /// then judged only on leaving), [`State::Far`] for one not yet accepted.
    pub fn new(params: Params, state: State) -> Self {
        Self {
            params,
            state,
            ema: None,
            last_sample: None,
            pending_since: None,
            cooldown_until: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Current smoothed value, if any sample has been seen since the last reset.
    pub fn smoothed(&self) -> Option<f64> {
        self.ema
    }

    /// Whether the cool-down still forbids becoming near at `now`.
    pub fn cooling_down(&self, now: Timestamp) -> bool {
        self.cooldown_until.is_some_and(|t| now < t)
    }

    /// Feed a reading taken at `now`. Returns a transition if one is due.
    pub fn sample(&mut self, now: Timestamp, value: f64) -> Option<Transition> {
        // A long silence before this sample still counts as stale, whatever the value.
        let stale = self.tick(now);
        let ema = match (self.ema, self.last_sample) {
            (Some(prev), Some(last)) => {
                let dt = now.saturating_sub(last).as_secs_f64();
                let alpha = 1.0 - (-dt / self.params.tau.as_secs_f64()).exp();
                prev + alpha * (value - prev)
            }
            _ => value,
        };
        self.ema = Some(ema);
        self.last_sample = Some(now);
        let timed = self.update_pending(now, ema);
        stale.or(timed)
    }

    /// Advance time without a sample: fires dwell expiry and the stale timeout.
    pub fn tick(&mut self, now: Timestamp) -> Option<Transition> {
        if let Some(last) = self.last_sample
            && now.saturating_sub(last) >= self.params.stale_after
        {
            // Forget the stale smoothing; the next sample starts afresh.
            self.ema = None;
            self.last_sample = None;
            self.pending_since = None;
            return self.enter(State::Far, Reason::Stale, now);
        }
        let since = self.pending_since?;
        let dwell = match self.state {
            State::Far => self.params.near_dwell,
            State::Near => self.params.far_dwell,
        };
        if now.saturating_sub(since) < dwell {
            return None;
        }
        match self.state {
            State::Far if self.cooling_down(now) => None,
            State::Far => self.enter(State::Near, Reason::Dwell, now),
            State::Near => self.enter(State::Far, Reason::Dwell, now),
        }
    }

    /// Forget all history, e.g. when the speaker disconnected for external reasons. Keeps
    /// the cool-down, so a speaker we dropped cannot dodge it by reconnecting.
    pub fn reset(&mut self, state: State) {
        self.state = state;
        self.ema = None;
        self.last_sample = None;
        self.pending_since = None;
    }

    /// Start (or restart) the cool-down at `now`, e.g. after we disconnected the speaker
    /// for being too far.
    pub fn start_cooldown(&mut self, now: Timestamp) {
        self.cooldown_until = Some(now + self.params.cooldown);
    }

    /// Track how long the smoothed value has been past the threshold for leaving the
    /// current state, and fire if the dwell is already satisfied.
    fn update_pending(&mut self, now: Timestamp, ema: f64) -> Option<Transition> {
        let past = match self.state {
            State::Far => ema >= self.params.near,
            State::Near => ema < self.params.far,
        };
        if !past {
            self.pending_since = None;
            return None;
        }
        self.pending_since.get_or_insert(now);
        self.tick(now)
    }

    fn enter(&mut self, to: State, reason: Reason, now: Timestamp) -> Option<Transition> {
        if self.state == to {
            return None;
        }
        self.state = to;
        self.pending_since = None;
        if to == State::Far {
            self.cooldown_until = Some(now + self.params.cooldown);
        }
        Some(Transition { to, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: f64) -> Timestamp {
        Duration::from_secs_f64(s)
    }

    fn dbm() -> Params {
        Params::dbm(&ProximityConfig::default())
    }

    fn mgmt() -> Params {
        Params::mgmt(&ProximityConfig::default())
    }

    /// Feed `(time, value)` pairs, returning every transition with its time.
    fn run(tr: &mut Tracker, trace: &[(f64, f64)]) -> Vec<(f64, Transition)> {
        trace
            .iter()
            .filter_map(|&(time, v)| tr.sample(t(time), v).map(|x| (time, x)))
            .collect()
    }

    /// Deterministic noise in -1.0..1.0 (LCG), so tests need no rand dependency.
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
        }
    }

    #[test]
    fn becomes_near_only_after_the_dwell() {
        let mut tr = Tracker::new(dbm(), State::Far);
        // Defaults: near at -68 dBm for 2 s.
        let trace: Vec<_> = (0..10).map(|i| (f64::from(i) * 0.5, -60.0)).collect();
        let got = run(&mut tr, &trace);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 2.0, "first sample at 0.0 starts the dwell");
        assert_eq!(got[0].1.to, State::Near);
        assert_eq!(got[0].1.reason, Reason::Dwell);
    }

    #[test]
    fn tick_fires_the_dwell_without_new_samples() {
        let mut tr = Tracker::new(dbm(), State::Far);
        assert_eq!(tr.sample(t(0.0), -60.0), None);
        assert_eq!(tr.tick(t(1.9)), None);
        assert_eq!(tr.tick(t(2.0)).map(|x| x.to), Some(State::Near));
        assert_eq!(tr.tick(t(3.0)), None, "fires once");
    }

    #[test]
    fn leaves_only_after_the_far_dwell() {
        let mut tr = Tracker::new(dbm(), State::Near);
        // Far at -80 for 5 s; the EMA needs a moment to cross the threshold first.
        let trace: Vec<_> = (0..40).map(|i| (f64::from(i) * 0.5, -90.0)).collect();
        let got = run(&mut tr, &trace);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.to, State::Far);
        // The very first sample seeds the EMA at -90, so the dwell runs from t = 0.
        assert_eq!(got[0].0, 5.0);
    }

    #[test]
    fn a_dip_shorter_than_the_dwell_is_ignored() {
        let mut tr = Tracker::new(dbm(), State::Near);
        let mut trace = vec![];
        for i in 0..20 {
            trace.push((f64::from(i) * 0.5, -60.0));
        }
        // Two seconds of very weak signal, then back.
        for i in 20..24 {
            trace.push((f64::from(i) * 0.5, -100.0));
        }
        for i in 24..60 {
            trace.push((f64::from(i) * 0.5, -60.0));
        }
        assert!(run(&mut tr, &trace).is_empty());
        assert_eq!(tr.state(), State::Near);
    }

    #[test]
    fn jitter_on_the_connect_boundary_does_not_flap() {
        let mut tr = Tracker::new(dbm(), State::Far);
        let mut noise = Noise(1);
        // Mean exactly at the near threshold, +-8 dB of noise, a sample every 0.3 s for
        // ten minutes. The smoothed value wobbles around -68 but must not oscillate:
        // once near, it stays near because the far threshold is 12 dB lower.
        let trace: Vec<_> = (0..2000)
            .map(|i| (f64::from(i) * 0.3, -68.0 + 8.0 * noise.next()))
            .collect();
        let got = run(&mut tr, &trace);
        assert!(got.len() <= 1, "flapped: {got:?}");
    }

    #[test]
    fn jitter_on_the_disconnect_boundary_does_not_flap() {
        let mut tr = Tracker::new(dbm(), State::Near);
        let mut noise = Noise(7);
        let trace: Vec<_> = (0..2000)
            .map(|i| (f64::from(i) * 0.3, -80.0 + 8.0 * noise.next()))
            .collect();
        let got = run(&mut tr, &trace);
        // At most the single drop; it can never come back because of the cool-down
        // plus the 12 dB hysteresis band.
        assert!(got.len() <= 1, "flapped: {got:?}");
    }

    #[test]
    fn jitter_across_the_whole_band_never_flaps_with_a_cooldown() {
        let mut tr = Tracker::new(dbm(), State::Near);
        let mut noise = Noise(42);
        // Mean in the middle of the hysteresis band, large noise.
        let trace: Vec<_> = (0..4000)
            .map(|i| (f64::from(i) * 0.25, -74.0 + 12.0 * noise.next()))
            .collect();
        let got = run(&mut tr, &trace);
        for pair in got.windows(2) {
            assert!(
                pair[1].0 - pair[0].0 >= 2.0,
                "transitions too close: {pair:?}"
            );
        }
        assert!(got.len() <= 4, "too many transitions: {got:?}");
    }

    #[test]
    fn ema_is_independent_of_the_sampling_rate() {
        let tau = dbm().tau.as_secs_f64();
        let mut fast = Tracker::new(dbm(), State::Near);
        let mut slow = Tracker::new(dbm(), State::Near);
        fast.sample(t(0.0), -50.0);
        slow.sample(t(0.0), -50.0);
        // A step to -90: sample every 0.1 s versus every 1 s, compared at t = 6.
        for i in 1..=60 {
            fast.sample(t(f64::from(i) * 0.1), -90.0);
        }
        for i in 1..=6 {
            slow.sample(t(f64::from(i)), -90.0);
        }
        let expected = -90.0 + 40.0 * (-6.0 / tau).exp();
        assert!((slow.smoothed().unwrap() - expected).abs() < 1e-9);
        // The fast one stepped at 0.1 instead of 0, so it is a hair further along.
        assert!((fast.smoothed().unwrap() - expected).abs() < 40.0 * (0.1 / tau));
    }

    #[test]
    fn no_samples_for_stale_after_means_far() {
        let mut tr = Tracker::new(dbm(), State::Near);
        tr.sample(t(0.0), -50.0);
        assert_eq!(tr.tick(t(29.9)), None);
        let got = tr.tick(t(30.0)).unwrap();
        assert_eq!((got.to, got.reason), (State::Far, Reason::Stale));
        assert_eq!(tr.smoothed(), None, "stale smoothing is forgotten");
    }

    #[test]
    fn a_late_sample_reports_staleness_first() {
        let mut tr = Tracker::new(dbm(), State::Near);
        tr.sample(t(0.0), -50.0);
        let got = tr.sample(t(45.0), -50.0).unwrap();
        assert_eq!((got.to, got.reason), (State::Far, Reason::Stale));
        // The late sample re-seeded the EMA instead of blending with the old value.
        assert_eq!(tr.smoothed(), Some(-50.0));
    }

    #[test]
    fn silence_from_the_start_is_not_stale() {
        // Nothing seen yet: there is no "last sample" to be stale against.
        let mut tr = Tracker::new(dbm(), State::Far);
        assert_eq!(tr.tick(t(1000.0)), None);
    }

    #[test]
    fn cooldown_blocks_reaccept_until_it_expires() {
        let mut tr = Tracker::new(dbm(), State::Near);
        let drop = run(&mut tr, &[(0.0, -95.0), (6.0, -95.0)]);
        assert_eq!(drop.len(), 1);
        assert_eq!(drop[0].1.to, State::Far);
        // Cool-down is 30 s from the drop at t = 6. Strong signal straight away:
        assert_eq!(tr.sample(t(8.0), -50.0), None);
        let mid = run(&mut tr, &[(12.0, -50.0), (20.0, -50.0), (35.0, -50.0)]);
        assert!(mid.is_empty(), "re-accepted during cool-down: {mid:?}");
        assert!(tr.cooling_down(t(35.0)));
        // At t = 36 the cool-down is over and the dwell has long been satisfied.
        let got = tr.sample(t(36.0), -50.0).unwrap();
        assert_eq!(got.to, State::Near);
    }

    #[test]
    fn reset_keeps_the_cooldown() {
        let mut tr = Tracker::new(dbm(), State::Near);
        tr.start_cooldown(t(0.0));
        tr.reset(State::Far);
        assert!(tr.cooling_down(t(10.0)));
        assert!(!tr.cooling_down(t(30.0)));
    }

    #[test]
    fn mgmt_walk_away_and_back_like_the_m1_spike() {
        // The M1 walk, one reading per ~3 s: 0 -5 -11 -20 -24 -27 -30 -26 -18 -8 0.
        let walk = [
            0, -5, -11, -20, -24, -27, -30, -30, -27, -18, -8, 0, 0, 0, 0, 0,
        ];
        let trace: Vec<_> = walk
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as f64 * 3.0, f64::from(v)))
            .collect();
        let mut tr = Tracker::new(mgmt(), State::Near);
        let got = run(&mut tr, &trace);
        let kinds: Vec<_> = got.iter().map(|(_, x)| x.to).collect();
        // It leaves once, and is only re-accepted after the cool-down. The walk ends ~17 s
        // after the drop, so it must not come back within the trace.
        assert_eq!(kinds, [State::Far], "{got:?}");
        // Dropped well after the signal crossed -25 (at 15 s), not on the first dip.
        assert!(got[0].0 > 18.0, "dropped too eagerly: {got:?}");
    }

    #[test]
    fn mgmt_reaccepts_after_cooldown_when_back_in_the_golden_range() {
        let mut tr = Tracker::new(mgmt(), State::Near);
        let mut trace: Vec<(f64, f64)> = (0..8).map(|i| (f64::from(i) * 3.0, -40.0)).collect();
        // Back at 0 dB from t = 24, readings every 3 s for another minute.
        trace.extend((8..30).map(|i| (f64::from(i) * 3.0, 0.0)));
        let got = run(&mut tr, &trace);
        let kinds: Vec<_> = got.iter().map(|(_, x)| x.to).collect();
        assert_eq!(kinds, [State::Far, State::Near], "{got:?}");
        let (dropped, back) = (got[0].0, got[1].0);
        assert!(back - dropped >= 30.0, "cool-down ignored: {got:?}");
    }

    #[test]
    fn mgmt_does_not_reaccept_between_the_thresholds() {
        // -18 dB is better than the -25 disconnect threshold but not the -10 re-accept one.
        let mut tr = Tracker::new(mgmt(), State::Near);
        let mut trace: Vec<(f64, f64)> = (0..8).map(|i| (f64::from(i) * 3.0, -40.0)).collect();
        trace.extend((8..60).map(|i| (f64::from(i) * 3.0, -18.0)));
        let got = run(&mut tr, &trace);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].1.to, State::Far);
    }

    #[test]
    fn params_follow_the_config() {
        let c = ProximityConfig::default();
        let d = Params::dbm(&c);
        assert_eq!((d.near, d.far), (-68.0, -80.0));
        assert_eq!(d.far_dwell, Duration::from_secs(5));
        let m = Params::mgmt(&c);
        assert_eq!((m.near, m.far), (-10.0, -25.0));
        assert_eq!(m.far_dwell, Duration::from_secs(6));
        assert_eq!(m.stale_after, d.stale_after);
        assert_eq!(m.cooldown, d.cooldown);
    }
}
