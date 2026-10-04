//! The `run` command: ties Bluetooth events, proximity filters, per-speaker state machines
//! and the audio engine together. One task owns every speaker's state; the slow Bluetooth
//! calls (pair, connect, disconnect, link RSSI) run in spawned tasks and report back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bluer::Address;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::audio::{AudioCommand, AudioEngine, AudioEvent, SinkRef, decode_file};
use crate::bluetooth::adapter::{Bluetooth, BtEvent};
use crate::bluetooth::candidate::{A2DP_SINK, Verdict, evaluate, matches};
use crate::bluetooth::device::{DeviceSnapshot, connect, disconnect};
use crate::bluetooth::mgmt::MgmtError;
use crate::bluetooth::pairing::Pairing;
use crate::bluetooth::rssi::{MgmtRssi, RssiSource};
use crate::config::Config;
use crate::proximity::{Params, State as Prox, Timestamp, Tracker};
use crate::report::{Item, describe, label, status_line};
use crate::speaker::{Action, Event, Machine, Timing};

/// How often deadlines and proximity timers are checked.
const TICK: Duration = Duration::from_secs(1);
/// How often the summary of who is playing is printed.
const STATUS_EVERY: Duration = Duration::from_secs(30);
/// How often the link RSSI of connected speakers is read.
const LINK_POLL: Duration = Duration::from_secs(3);
/// Volume ramp when a speaker is linked.
const RAMP: Duration = Duration::from_millis(500);
/// A speaker that is not connected and was not heard for this long is forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(600);
/// A paired speaker that is on but disconnected gives no discovery RSSI, so with none for this
/// long it is probed: connected, then judged by its link RSSI.
const PROBE_AFTER: Duration = Duration::from_secs(45);
/// Minimum time between probes of one speaker.
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// A drop within this long of pairing is not taken as the user turning the speaker off.
const FRESH_PAIR: Duration = Duration::from_secs(60);
/// A probed speaker has this long to show a near link before it is released again.
const PROBE_GRACE: Duration = Duration::from_secs(15);

/// Results of the spawned Bluetooth calls.
enum Done {
    Pair(Address, bool),
    Connect(Address, bool),
    Disconnect(Address, bool),
    Link(Address, Option<f64>),
}

struct Entry {
    snapshot: DeviceSnapshot,
    machine: Machine,
    /// Proximity from discovery RSSI (true dBm), used while not connected.
    dbm: Tracker,
    /// Proximity from the live link (relative dB), used while connected.
    link: Tracker,
    was_connected: bool,
    /// We told the machine to want this speaker.
    granted: bool,
    /// Connected on a hunch (no RSSI to go on); the link RSSI has to confirm it is near.
    probing: bool,
    probe_until: Timestamp,
    next_probe: Timestamp,
    /// When we last paired it. Speakers often drop the link right after pairing.
    paired_at: Option<Timestamp>,
    /// Already logged that it is waiting for a free slot.
    blocked: bool,
    /// Last time a discovery RSSI arrived.
    last_rssi: Option<Timestamp>,
    link_poll_inflight: bool,
    last_seen: Instant,
    /// Connect threshold (dBm) in force for this speaker, and when we last said it was too weak.
    connect_dbm: f64,
    weak_logged: Option<Instant>,
}

impl Entry {
    fn name(&self) -> &str {
        self.snapshot.name.as_deref().unwrap_or("?")
    }

    fn near(&self, now: Timestamp) -> bool {
        if self.probing {
            return if self.machine.is_connected() {
                self.link.state() == Prox::Near || now < self.probe_until
            } else {
                self.machine.is_engaged()
            };
        }
        let tracker = if self.machine.is_connected() {
            &self.link
        } else {
            &self.dbm
        };
        tracker.state() == Prox::Near
    }

    /// Debug note, at most every 10 s, that the signal is too weak to connect.
    fn note_weak(&mut self, dbm: i16) {
        if self.machine.is_engaged() || f64::from(dbm) >= self.connect_dbm {
            return;
        }
        if self
            .weak_logged
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.weak_logged = Some(Instant::now());
        tracing::debug!(
            "{} [{}]: signal {dbm} dBm is too weak to connect (needs {:.0} dBm or better)",
            self.name(),
            self.snapshot.address,
            self.connect_dbm
        );
    }

    /// Switch proximity source when the connection state changes.
    fn sync_connected(&mut self, now: Timestamp, link_rssi: bool) {
        let connected = self.machine.is_connected();
        if connected == self.was_connected {
            return;
        }
        self.was_connected = connected;
        if connected {
            if self.probing {
                self.probe_until = now + PROBE_GRACE;
            }
            // A speaker we connected is near by definition; one that connected on its own
            // has to prove it. Without link RSSI there is nothing to prove it with.
            let start = if (self.granted && !self.probing) || !link_rssi {
                Prox::Near
            } else {
                Prox::Far
            };
            self.link.reset(start);
        } else {
            self.dropped(now);
        }
    }

    /// The link ended. Discovery RSSI is a different scale: start over, and do not bounce
    /// straight back in, unless the speaker has only just been paired (it tends to drop the
    /// link by itself then; reconnect soon instead).
    fn dropped(&mut self, now: Timestamp) {
        self.dbm.reset(Prox::Far);
        if self
            .paired_at
            .is_some_and(|t| now.saturating_sub(t) < FRESH_PAIR)
        {
            self.next_probe = now + Duration::from_secs(3);
            self.last_rssi = None;
        } else {
            self.dbm.start_cooldown(now);
        }
    }
}

/// One line saying what a device is and why it does not qualify.
fn ignored_text(s: &DeviceSnapshot, verdict: Verdict) -> String {
    let why = match verdict {
        Verdict::Denied => "on the deny list",
        Verdict::NotAllowed => "not on the allow list",
        Verdict::VideoDevice => "a TV or other video device",
        Verdict::NotAudio => "not an audio speaker",
        Verdict::Accept => "accepted",
    };
    let class = s
        .class
        .map_or("unknown".to_string(), |c| format!("0x{c:06x}"));
    let a2dp = if s.uuids.contains(&A2DP_SINK) {
        "yes"
    } else {
        "no"
    };
    let rssi = s.rssi.map_or(String::new(), |r| format!(", {r} dBm"));
    format!(
        "{} [{}]: {why} (class {class}, A2DP sink {a2dp}{rssi})",
        s.name.as_deref().unwrap_or("?"),
        s.address
    )
}

struct Orchestrator {
    config: Config,
    bt: Bluetooth,
    pairing: Arc<Pairing>,
    engine: AudioEngine,
    link_rssi: Option<Arc<MgmtRssi>>,
    entries: BTreeMap<Address, Entry>,
    /// Bluetooth sinks currently in the PipeWire graph.
    sinks: HashSet<Address>,
    done_tx: mpsc::UnboundedSender<Done>,
    /// Devices ruled out, with the verdict already logged (logged again only if it changes).
    ignored: HashMap<Address, Verdict>,
    epoch: Instant,
}

pub async fn run(config: Config, file: &Path) -> Result<()> {
    let pcm = decode_file(file)?;
    tracing::info!(
        "decoded {}: {:.1}s at {} Hz",
        file.display(),
        pcm.frames() as f32 / pcm.rate as f32,
        pcm.rate
    );
    let (engine, mut audio_events) = AudioEngine::start(pcm)?;
    let (bt, mut bt_events) = Bluetooth::start(&config.discovery).await?;
    let pairing = Arc::new(Pairing::start(bt.session(), bt.adapter().clone(), &config).await?);
    let link_rssi = match MgmtRssi::new(bt.index()) {
        Ok(m) => Some(Arc::new(m)),
        Err(MgmtError::PermissionDenied) => {
            tracing::warn!(
                "no CAP_NET_ADMIN: cannot read the link RSSI of connected speakers, so \
                 they will not be released when you walk away and speakers that connect \
                 on their own are adopted without a proximity check \
                 (setcap cap_net_admin+ep on the binary, or run it with sudo)"
            );
            None
        }
        Err(e) => {
            tracing::warn!("management socket unavailable: {e}");
            None
        }
    };
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let mut o = Orchestrator {
        config,
        bt,
        pairing,
        engine,
        link_rssi,
        entries: BTreeMap::new(),
        sinks: HashSet::new(),
        done_tx,
        ignored: HashMap::new(),
        epoch: Instant::now(),
    };

    let mut tick = tokio::time::interval(TICK);
    let mut poll = tokio::time::interval(LINK_POLL);
    let mut summary =
        tokio::time::interval_at(tokio::time::Instant::now() + STATUS_EVERY, STATUS_EVERY);
    let mut sigterm = signal(SignalKind::terminate())?;
    tracing::info!("running; Ctrl-C to stop");
    let mut failure = None;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            _ = tick.tick() => o.on_tick(),
            _ = poll.tick() => o.poll_links(),
            _ = summary.tick() => tracing::info!("{}", o.status_line()),
            ev = bt_events.recv() => match ev {
                Some(BtEvent::Lost) | None => {
                    failure = Some("lost bluetoothd");
                    break;
                }
                Some(ev) => o.on_bluetooth(ev),
            },
            ev = audio_events.recv() => match ev {
                Some(ev) => o.on_audio(ev),
                None => {
                    failure = Some("lost PipeWire");
                    break;
                }
            },
            Some(done) = done_rx.recv() => o.on_done(done),
        }
        o.reconcile();
    }
    o.shutdown().await;
    match failure {
        // A non-zero exit lets a supervisor (systemd Restart=on-failure) start from scratch,
        // which rebuilds all state once the daemon is back.
        Some(what) => Err(anyhow::anyhow!(
            "{what}; exiting so that a supervisor can restart us"
        )),
        None => Ok(()),
    }
}

impl Orchestrator {
    fn now(&self) -> Timestamp {
        self.epoch.elapsed()
    }

    fn on_bluetooth(&mut self, event: BtEvent) {
        let now = self.now();
        match event {
            BtEvent::Updated(snapshot) => {
                let address = snapshot.address;
                if !self.entries.contains_key(&address) {
                    let verdict = evaluate(&snapshot.info(), &self.config.allow, &self.config.deny);
                    if !verdict.is_accepted() {
                        if self.ignored.insert(address, verdict) != Some(verdict) {
                            tracing::debug!("ignoring {}", ignored_text(&snapshot, verdict));
                        }
                        return;
                    }
                    self.ignored.remove(&address);
                    let entry = self.new_entry(snapshot.clone());
                    tracing::info!(
                        "{} [{address}]: found ({})",
                        entry.name(),
                        if snapshot.paired {
                            "paired"
                        } else {
                            "not paired"
                        }
                    );
                    self.entries.insert(address, entry);
                    if self.sinks.contains(&address) {
                        self.step(address, Event::SinkAppeared);
                    }
                }
                let Some(entry) = self.entries.get_mut(&address) else {
                    return;
                };
                entry.last_seen = Instant::now();
                if !snapshot.connected
                    && entry.dbm.smoothed().is_none()
                    && let Some(rssi) = snapshot.rssi
                {
                    entry.dbm.sample(now, f64::from(rssi));
                    entry.last_rssi = Some(now);
                }
                let (paired, connected) = (snapshot.paired, snapshot.connected);
                entry.snapshot = snapshot;
                self.step(address, Event::Seen { paired });
                self.step(address, Event::Connected(connected));
            }
            BtEvent::Rssi { address, dbm } => {
                tracing::trace!(%address, dbm, "rssi");
                if let Some(entry) = self.entries.get_mut(&address) {
                    entry.last_seen = Instant::now();
                    entry.snapshot.rssi = Some(dbm);
                    if !entry.machine.is_connected() {
                        entry.dbm.sample(now, f64::from(dbm));
                        entry.last_rssi = Some(now);
                        entry.note_weak(dbm);
                    }
                }
            }
            BtEvent::Removed(address) => {
                self.ignored.remove(&address);
                self.step(address, Event::Gone);
                self.entries.remove(&address);
            }
            BtEvent::Lost => {}
        }
    }

    fn new_entry(&self, snapshot: DeviceSnapshot) -> Entry {
        let mut params = Params::dbm(&self.config.proximity);
        if let Some(o) = self.override_for(&snapshot) {
            if let Some(v) = o.connect_rssi {
                params.near = f64::from(v);
            }
            if let Some(v) = o.disconnect_rssi {
                params.far = f64::from(v);
            }
        }
        Entry {
            machine: Machine::new(self.config.auto_pair, Timing::default()),
            dbm: Tracker::new(params, Prox::Far),
            link: Tracker::new(Params::mgmt(&self.config.proximity), Prox::Far),
            was_connected: false,
            granted: false,
            probing: false,
            probe_until: Timestamp::ZERO,
            next_probe: Timestamp::ZERO,
            paired_at: None,
            blocked: false,
            last_rssi: None,
            link_poll_inflight: false,
            last_seen: Instant::now(),
            connect_dbm: params.near,
            weak_logged: None,
            snapshot,
        }
    }

    fn override_for(&self, snapshot: &DeviceSnapshot) -> Option<&crate::config::DeviceOverride> {
        self.config
            .devices
            .iter()
            .find(|d| matches(&d.matcher, &snapshot.info()))
    }

    fn on_audio(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::SinkAppeared(info) => {
                if let Some(a) = info.address {
                    self.sinks.insert(a);
                    self.step(a, Event::SinkAppeared);
                }
            }
            AudioEvent::SinkRemoved(info) => {
                if let Some(a) = info.address {
                    self.sinks.remove(&a);
                    self.step(a, Event::SinkRemoved);
                }
            }
            AudioEvent::Unlinked(info) => {
                if let Some(a) = info.address {
                    self.step(a, Event::Unlinked);
                }
            }
            AudioEvent::Linked(info) => tracing::debug!(sink = %info.name, "linked"),
        }
    }

    fn on_done(&mut self, done: Done) {
        let now = self.now();
        match done {
            Done::Pair(a, ok) => {
                if let (true, Some(e)) = (ok, self.entries.get_mut(&a)) {
                    e.paired_at = Some(now);
                }
                self.step(a, Event::PairResult(ok));
            }
            Done::Connect(a, ok) => self.step(a, Event::ConnectResult(ok)),
            Done::Disconnect(a, ok) => self.step(a, Event::DisconnectResult(ok)),
            Done::Link(a, value) => {
                if let Some(entry) = self.entries.get_mut(&a) {
                    entry.link_poll_inflight = false;
                    if let Some(v) = value {
                        entry.link.sample(now, v);
                    }
                }
            }
        }
    }

    fn on_tick(&mut self) {
        let now = self.now();
        let link_rssi = self.link_rssi.is_some();
        for entry in self.entries.values_mut() {
            if entry.machine.is_connected() {
                // Without link RSSI there is no data to age out, so leave it alone.
                if link_rssi {
                    entry.link.tick(now);
                }
            } else {
                entry.dbm.tick(now);
            }
        }
        let addresses: Vec<Address> = self.entries.keys().copied().collect();
        for a in addresses {
            self.step(a, Event::Tick);
        }
        self.start_probes(now);
        // Forget speakers that were not heard from for a long time.
        let stale: Vec<Address> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.machine.is_connected() && e.last_seen.elapsed() > FORGET_AFTER)
            .map(|(a, _)| *a)
            .collect();
        for a in stale {
            tracing::info!(address = %a, "speaker forgotten (not heard for a long time)");
            self.step(a, Event::Gone);
            self.entries.remove(&a);
        }
    }

    /// Connect to paired, idle speakers that give no discovery RSSI, one by one while slots
    /// are free; the link RSSI then confirms or releases them.
    fn start_probes(&mut self, now: Timestamp) {
        for (address, e) in self.entries.iter_mut() {
            if e.probing && e.machine.is_connected() && e.link.state() == Prox::Near {
                tracing::info!("{} [{address}]: it is near, keeping it", e.name());
                e.probing = false;
            }
        }
        if self.entries.values().any(|e| e.probing) {
            return;
        }
        let granted = self.entries.values().filter(|e| e.granted).count();
        if granted >= self.config.max_connected {
            return;
        }
        let due = self.entries.iter().find(|(_, e)| {
            e.snapshot.paired
                && !e.granted
                && !e.machine.is_connected()
                && e.machine.state() == crate::speaker::State::InRange
                && e.next_probe <= now
                && !e.dbm.cooling_down(now)
                && e.last_rssi
                    .is_none_or(|t| now.saturating_sub(t) > PROBE_AFTER)
        });
        let Some((&address, _)) = due else { return };
        let e = self.entries.get_mut(&address).expect("just found");
        tracing::debug!(%address, name = e.name(), "no discovery RSSI: probing with a connect");
        e.probing = true;
        e.granted = true;
        e.next_probe = now + PROBE_INTERVAL;
        self.step(address, Event::Want(true));
    }

    /// One line: who is playing, and the state of the other speakers.
    fn status_line(&self) -> String {
        let now = self.now();
        let items: Vec<Item> = self
            .entries
            .values()
            .map(|e| Item {
                name: e.name().to_string(),
                playing: e.machine.state() == crate::speaker::State::Linked,
                label: label(
                    e.machine.state(),
                    e.snapshot.paired,
                    e.machine.is_connected(),
                    e.blocked,
                    e.probing,
                    now,
                ),
            })
            .collect();
        status_line(&items, self.config.max_connected)
    }

    fn poll_links(&mut self) {
        let Some(rssi) = self.link_rssi.clone() else {
            return;
        };
        for (&address, entry) in &mut self.entries {
            if !entry.machine.is_connected() || entry.link_poll_inflight {
                continue;
            }
            entry.link_poll_inflight = true;
            let (rssi, tx) = (rssi.clone(), self.done_tx.clone());
            tokio::spawn(async move {
                let value = rssi.read(address).await;
                let _ = tx.send(Done::Link(address, value));
            });
        }
    }

    /// Decide which speakers we want: every near one, strongest first, up to
    /// `max_connected`. Speakers we already hold keep their slot while they stay near.
    fn reconcile(&mut self) {
        let now = self.now();
        let mut changes: Vec<(Address, bool)> = Vec::new();
        for (&a, e) in self.entries.iter_mut() {
            if e.granted && !e.near(now) {
                e.granted = false;
                if e.probing {
                    // Did not pan out; try again later, not right away.
                    e.probing = false;
                    e.next_probe = now + PROBE_INTERVAL * 2;
                }
                changes.push((a, false));
            }
        }
        let mut free = self
            .config
            .max_connected
            .saturating_sub(self.entries.values().filter(|e| e.granted).count());
        let mut candidates: Vec<(Address, f64)> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.granted && e.near(now))
            .map(|(&a, e)| (a, e.dbm.smoothed().unwrap_or(f64::MIN)))
            .collect();
        candidates.sort_by(|x, y| y.1.total_cmp(&x.1));
        for (a, _) in candidates {
            if free == 0 {
                if let Some(e) = self.entries.get_mut(&a)
                    && !e.blocked
                {
                    e.blocked = true;
                    tracing::info!(
                        "{} [{a}]: near, waiting for a free slot (max_connected)",
                        e.name()
                    );
                }
                continue;
            }
            free -= 1;
            if let Some(e) = self.entries.get_mut(&a) {
                e.granted = true;
                e.blocked = false;
            }
            changes.push((a, true));
        }
        for (a, want) in changes {
            self.step(a, Event::Want(want));
        }
    }

    /// Feed one event to a speaker's machine and execute what it asks for.
    fn step(&mut self, address: Address, event: Event) {
        let now = self.now();
        let link_rssi = self.link_rssi.is_some();
        let volume = self.volume_for(address);
        let Some(entry) = self.entries.get_mut(&address) else {
            return;
        };
        let before = entry.machine.state();
        let actions = entry.machine.handle(event, now);
        let after = entry.machine.state();
        if std::mem::discriminant(&before) != std::mem::discriminant(&after) {
            tracing::debug!(%address, name = entry.name(), "{before:?} -> {after:?} on {event:?}");
        }
        if let Some(text) = describe(before, after, event, entry.probing, now) {
            tracing::info!("{} [{address}]: {text}", entry.name());
        }
        entry.sync_connected(now, link_rssi);
        if actions.contains(&Action::ExternalDrop) {
            entry.dropped(now);
        }
        for action in actions {
            tracing::debug!(%address, ?action);
            self.execute(address, action, volume);
        }
    }

    fn volume_for(&self, address: Address) -> f32 {
        self.entries
            .get(&address)
            .and_then(|e| self.override_for(&e.snapshot))
            .and_then(|o| o.volume)
            .unwrap_or(self.config.default_volume)
    }

    fn execute(&self, address: Address, action: Action, volume: f32) {
        let tx = self.done_tx.clone();
        let adapter = self.bt.adapter().clone();
        match action {
            Action::Pair => {
                let pairing = self.pairing.clone();
                tokio::spawn(async move {
                    let _discovery = hold_discovery(&adapter).await;
                    let result = pairing.ensure_paired(address).await;
                    if let Err(e) = &result {
                        tracing::warn!(%address, "pairing failed: {e}");
                    }
                    let _ = tx.send(Done::Pair(address, result.is_ok()));
                });
            }
            Action::Connect => {
                tokio::spawn(async move {
                    let ok = connect_during_discovery(&adapter, address).await;
                    let _ = tx.send(Done::Connect(address, ok));
                });
            }
            Action::Disconnect => {
                tokio::spawn(async move {
                    let ok = match adapter.device(address) {
                        Ok(d) => disconnect(&d)
                            .await
                            .inspect_err(|e| tracing::warn!(%address, "disconnect failed: {e}"))
                            .is_ok(),
                        Err(_) => false,
                    };
                    let _ = tx.send(Done::Disconnect(address, ok));
                });
            }
            Action::Link => {
                let sink = SinkRef::Address(address);
                let _ = self.engine.send(AudioCommand::SetVolume {
                    sink: sink.clone(),
                    volume,
                    ramp: RAMP,
                });
                let _ = self.engine.send(AudioCommand::Link(sink));
            }
            Action::Unlink => {
                let _ = self
                    .engine
                    .send(AudioCommand::Unlink(SinkRef::Address(address)));
            }
            Action::ExternalDrop => {}
        }
    }

    /// Fade out, then release the speakers we hold.
    async fn shutdown(self) {
        tracing::info!("shutting down");
        let held: Vec<Address> = self
            .entries
            .iter()
            .filter(|(_, e)| e.machine.is_engaged())
            .map(|(a, _)| *a)
            .collect();
        for &a in &held {
            self.execute(a, Action::Unlink, 0.0);
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        let adapter = self.bt.adapter().clone();
        for a in held {
            if let Ok(d) = adapter.device(a) {
                let _ = tokio::time::timeout(Duration::from_secs(5), disconnect(&d)).await;
            }
        }
    }
}

/// Hold a discovery session while we page the speaker: `Connect` and `Pair` fail with a page
/// timeout unless discovery is running (found in M5), and the duty cycle may switch it off
/// mid-attempt. BlueZ keeps scanning while any client holds a session.
async fn hold_discovery(adapter: &bluer::Adapter) -> Option<impl Sized + use<>> {
    adapter
        .discover_devices()
        .await
        .inspect_err(|e| tracing::debug!("holding discovery: {e}"))
        .ok()
}

async fn connect_during_discovery(adapter: &bluer::Adapter, address: Address) -> bool {
    let Ok(device) = adapter.device(address) else {
        return false;
    };
    let _discovery = hold_discovery(adapter).await;
    match connect(&device).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(%address, "connect failed: {e}");
            false
        }
    }
}
