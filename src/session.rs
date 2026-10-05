//! What `run` tells the user when it stops: how many speakers it saw, how many it played on,
//! and for how long. Pure bookkeeping, kept apart from the orchestrator so it can be
//! unit-tested; the caller passes the time in.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bluer::Address;

struct SpeakerStats {
    name: String,
    /// Times it reached the playing state.
    connections: u32,
    /// Failed attempts to pair or connect.
    failures: u32,
    /// Closed playing spans, added up.
    played: Duration,
    playing_since: Option<Instant>,
}

pub struct Session {
    started: Instant,
    speakers: BTreeMap<Address, SpeakerStats>,
    playing: usize,
    /// Time with at least one speaker playing, and when the current such span began.
    any_played: Duration,
    any_since: Option<Instant>,
}

impl Session {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            speakers: BTreeMap::new(),
            playing: 0,
            any_played: Duration::ZERO,
            any_since: None,
        }
    }

    fn stats(&mut self, address: Address) -> &mut SpeakerStats {
        self.speakers
            .entry(address)
            .or_insert_with(|| SpeakerStats {
                name: "?".into(),
                connections: 0,
                failures: 0,
                played: Duration::ZERO,
                playing_since: None,
            })
    }

    /// An audio speaker was found. Repeat sightings only refresh the name.
    pub fn seen(&mut self, address: Address, name: Option<&str>) {
        let stats = self.stats(address);
        if let Some(name) = name {
            stats.name = name.to_string();
        }
    }

    pub fn failed(&mut self, address: Address) {
        self.stats(address).failures += 1;
    }

    pub fn started_playing(&mut self, address: Address, now: Instant) {
        let stats = self.stats(address);
        if stats.playing_since.is_some() {
            return;
        }
        stats.playing_since = Some(now);
        stats.connections += 1;
        self.playing += 1;
        if self.playing == 1 {
            self.any_since = Some(now);
        }
    }

    pub fn stopped_playing(&mut self, address: Address, now: Instant) {
        let stats = self.stats(address);
        let Some(since) = stats.playing_since.take() else {
            return;
        };
        stats.played += now.saturating_duration_since(since);
        self.playing -= 1;
        if self.playing == 0
            && let Some(since) = self.any_since.take()
        {
            self.any_played += now.saturating_duration_since(since);
        }
    }

    /// Close every span still open, e.g. at shutdown.
    pub fn finish(&mut self, now: Instant) {
        let open: Vec<Address> = self
            .speakers
            .iter()
            .filter(|(_, s)| s.playing_since.is_some())
            .map(|(&a, _)| a)
            .collect();
        for a in open {
            self.stopped_playing(a, now);
        }
    }

    pub fn render(&self, now: Instant) -> String {
        let seen = self.speakers.len();
        let connected = self.speakers.values().filter(|s| s.connections > 0).count();
        let failures: u32 = self.speakers.values().map(|s| s.failures).sum();
        let total: Duration = self.speakers.values().map(|s| s.played).sum();
        let mut out = format!(
            "Session summary ({})\n  speakers seen: {seen}   played on: {connected}   \
             failed attempts: {failures}\n  total playing time: {} across {connected} {}; \
             at least one speaker playing: {}",
            format_duration(now.saturating_duration_since(self.started)),
            format_duration(total),
            if connected == 1 {
                "speaker"
            } else {
                "speakers"
            },
            format_duration(self.any_played),
        );
        for (address, s) in &self.speakers {
            out.push_str(&format!("\n    {} [{address}]  ", s.name));
            if s.connections == 0 {
                out.push_str("seen, never played");
            } else {
                out.push_str(&format!(
                    "{} {}  {}",
                    s.connections,
                    if s.connections == 1 {
                        "connection"
                    } else {
                        "connections"
                    },
                    format_duration(s.played)
                ));
            }
        }
        out
    }
}

/// `45s`, `12m 40s`, `1h 02m 05s`.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s:02}s"),
        _ => format!("{h}h {m:02}m {s:02}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> Address {
        Address::new([0, 0, 0, 0, 0, n])
    }

    fn secs(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    #[test]
    fn durations_are_formatted_compactly() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(760)), "12m 40s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m 05s");
    }

    #[test]
    fn spans_add_up_per_speaker() {
        let t0 = Instant::now();
        let mut s = Session::new(t0);
        s.seen(addr(1), Some("JBL"));
        s.started_playing(addr(1), secs(t0, 10));
        s.stopped_playing(addr(1), secs(t0, 40));
        s.started_playing(addr(1), secs(t0, 100));
        s.stopped_playing(addr(1), secs(t0, 120));
        let stats = &s.speakers[&addr(1)];
        assert_eq!(stats.connections, 2);
        assert_eq!(stats.played, Duration::from_secs(50));
    }

    #[test]
    fn overlap_counts_in_total_but_once_in_wall_clock() {
        let t0 = Instant::now();
        let mut s = Session::new(t0);
        s.started_playing(addr(1), secs(t0, 0));
        s.started_playing(addr(2), secs(t0, 10));
        s.stopped_playing(addr(1), secs(t0, 20));
        s.stopped_playing(addr(2), secs(t0, 30));
        let total: Duration = s.speakers.values().map(|x| x.played).sum();
        assert_eq!(total, Duration::from_secs(40));
        assert_eq!(s.any_played, Duration::from_secs(30));
    }

    #[test]
    fn finish_closes_open_spans_and_repeats_are_ignored() {
        let t0 = Instant::now();
        let mut s = Session::new(t0);
        s.started_playing(addr(1), secs(t0, 0));
        s.started_playing(addr(1), secs(t0, 5));
        s.finish(secs(t0, 60));
        s.finish(secs(t0, 90));
        s.stopped_playing(addr(1), secs(t0, 90));
        let stats = &s.speakers[&addr(1)];
        assert_eq!(stats.connections, 1);
        assert_eq!(stats.played, Duration::from_secs(60));
        assert_eq!(s.any_played, Duration::from_secs(60));
    }

    #[test]
    fn report_lists_each_speaker() {
        let t0 = Instant::now();
        let mut s = Session::new(t0);
        s.seen(addr(1), Some("JBL"));
        s.seen(addr(2), Some("Shokz"));
        s.failed(addr(2));
        s.started_playing(addr(1), secs(t0, 0));
        s.finish(secs(t0, 90));
        let text = s.render(secs(t0, 100));
        assert!(text.starts_with("Session summary (1m 40s)"));
        assert!(text.contains("speakers seen: 2   played on: 1   failed attempts: 1"));
        assert!(text.contains("total playing time: 1m 30s across 1 speaker;"));
        assert!(text.contains("JBL [00:00:00:00:00:01]  1 connection  1m 30s"));
        assert!(text.contains("Shokz [00:00:00:00:00:02]  seen, never played"));
    }
}
