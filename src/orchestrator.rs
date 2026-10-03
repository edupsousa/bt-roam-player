//! The `run` command: ties Bluetooth events, proximity filters, per-speaker state machines
//! and the audio engine together. One task owns every speaker's state; the slow Bluetooth
//! calls (pair, connect, disconnect, link RSSI) run in spawned tasks and report back.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bluer::Address;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::audio::{AudioCommand, AudioEngine, AudioEvent, SinkRef, decode_file};
use crate::bluetooth::adapter::{Bluetooth, BtEvent};
use crate::bluetooth::candidate::{evaluate, matches};
use crate::bluetooth::device::{DeviceSnapshot, connect, disconnect};
use crate::bluetooth::mgmt::MgmtError;
use crate::bluetooth::pairing::Pairing;
use crate::bluetooth::rssi::{MgmtRssi, RssiSource};
use crate::config::Config;
use crate::proximity::{Params, State as Prox, Timestamp, Tracker};
use crate::speaker::{Action, Event, Machine, Timing};

/// How often deadlines and proximity timers are checked.
const TICK: Duration = Duration::from_secs(1);
/// How often the link RSSI of connected speakers is read.
const LINK_POLL: Duration = Duration::from_secs(3);
/// Volume ramp when a speaker is linked.
const RAMP: Duration = Duration::from_millis(500);
/// A speaker that is not connected and was not heard for this long is forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(600);
/// `Connect` works only while discovery is running (DESIGN.md decision 3); wait at most
/// this long for the next discovery window.
const DISCOVERY_WAIT: Duration = Duration::from_secs(40);

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
    link_poll_inflight: bool,
    last_seen: Instant,
}

impl Entry {
    fn name(&self) -> &str {
        self.snapshot.name.as_deref().unwrap_or("?")
    }

    fn near(&self) -> bool {
        let tracker = if self.machine.is_connected() {
            &self.link
        } else {
            &self.dbm
        };
        tracker.state() == Prox::Near
    }

    /// Switch proximity source when the connection state changes.
    fn sync_connected(&mut self, now: Timestamp, link_rssi: bool) {
        let connected = self.machine.is_connected();
        if connected == self.was_connected {
            return;
        }
        self.was_connected = connected;
        if connected {
            // A speaker we connected is near by definition; one that connected on its own
            // has to prove it. Without link RSSI there is nothing to prove it with.
            let start = if self.granted || !link_rssi {
                Prox::Near
            } else {
                Prox::Far
            };
            self.link.reset(start);
        } else {
            // Discovery RSSI is a different scale: start over, and do not bounce straight
            // back in.
            self.dbm.reset(Prox::Far);
            self.dbm.start_cooldown(now);
        }
    }
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
        epoch: Instant::now(),
    };

    let mut tick = tokio::time::interval(TICK);
    let mut poll = tokio::time::interval(LINK_POLL);
    let mut sigterm = signal(SignalKind::terminate())?;
    tracing::info!("running; Ctrl-C to stop");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            _ = tick.tick() => o.on_tick(),
            _ = poll.tick() => o.poll_links(),
            Some(ev) = bt_events.recv() => o.on_bluetooth(ev),
            Some(ev) = audio_events.recv() => o.on_audio(ev),
            Some(done) = done_rx.recv() => o.on_done(done),
        }
        o.reconcile();
    }
    o.shutdown().await;
    Ok(())
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
                    if !evaluate(&snapshot.info(), &self.config.allow, &self.config.deny)
                        .is_accepted()
                    {
                        return;
                    }
                    let entry = self.new_entry(snapshot.clone());
                    tracing::info!(%address, name = entry.name(), "speaker seen");
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
                }
                let (paired, connected) = (snapshot.paired, snapshot.connected);
                entry.snapshot = snapshot;
                self.step(address, Event::Seen { paired });
                self.step(address, Event::Connected(connected));
            }
            BtEvent::Rssi { address, dbm } => {
                if let Some(entry) = self.entries.get_mut(&address) {
                    entry.last_seen = Instant::now();
                    entry.snapshot.rssi = Some(dbm);
                    if !entry.machine.is_connected() {
                        entry.dbm.sample(now, f64::from(dbm));
                    }
                }
            }
            BtEvent::Removed(address) => {
                self.step(address, Event::Gone);
                self.entries.remove(&address);
            }
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
            link_poll_inflight: false,
            last_seen: Instant::now(),
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
            Done::Pair(a, ok) => self.step(a, Event::PairResult(ok)),
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
        let mut changes: Vec<(Address, bool)> = Vec::new();
        for (&a, e) in self.entries.iter_mut() {
            if e.granted && !e.near() {
                e.granted = false;
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
            .filter(|(_, e)| !e.granted && e.near())
            .map(|(&a, e)| (a, e.dbm.smoothed().unwrap_or(f64::MIN)))
            .collect();
        candidates.sort_by(|x, y| y.1.total_cmp(&x.1));
        for (a, _) in candidates {
            if free == 0 {
                tracing::debug!(address = %a, "near, but max_connected is reached");
                break;
            }
            free -= 1;
            if let Some(e) = self.entries.get_mut(&a) {
                e.granted = true;
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
            tracing::info!(%address, name = entry.name(), "{before:?} -> {after:?} on {event:?}");
        }
        entry.sync_connected(now, link_rssi);
        if actions.contains(&Action::ExternalDrop) {
            entry.dbm.reset(Prox::Far);
            entry.dbm.start_cooldown(now);
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

/// `Connect` fails with a page timeout unless discovery is running (found in M5), so wait
/// for the discovery duty cycle to be on.
async fn connect_during_discovery(adapter: &bluer::Adapter, address: Address) -> bool {
    let Ok(device) = adapter.device(address) else {
        return false;
    };
    let started = Instant::now();
    while !adapter.is_discovering().await.unwrap_or(false) {
        if started.elapsed() > DISCOVERY_WAIT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    match connect(&device).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(%address, "connect failed: {e}");
            false
        }
    }
}
