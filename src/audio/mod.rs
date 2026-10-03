//! Audio engine: one looping playback stream, linked on demand to any PipeWire sink.
//!
//! The PipeWire main loop is not `Send`, so it lives on a dedicated thread. The rest of
//! the program talks to it through [`AudioEngine`]: commands go in over a
//! `pipewire::channel`, events come back over a tokio channel.
pub mod decode;
pub mod graph;
pub mod stream;
pub mod volume;

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::{
    context::ContextRc,
    core::CoreRc,
    device::{Device, DeviceListener},
    keys,
    link::Link,
    node::Node,
    properties::properties,
    registry::{GlobalObject, RegistryRc},
    spa::{param::ParamType, utils::dict::DictRef},
    types::ObjectType,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub use decode::{Pcm, decode_file};
pub use graph::{SinkInfo, SinkRef};

use graph::{Graph, Removed};
use volume::{Ramp, Route};

const TICK: Duration = Duration::from_millis(20);
/// WirePlumber may restore a saved volume shortly after a sink appears; write once more.
const REASSERT_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum AudioCommand {
    /// Keep our stream linked to this sink: link now if it exists, otherwise as soon as
    /// it appears. Stays in force across the sink disappearing and coming back.
    Link(SinkRef),
    /// Remove the link and stop wanting it.
    Unlink(SinkRef),
    /// Set the sink's volume (perceptual, 0.0..=1.0), ramping from the last value we
    /// wrote (or 0) over `ramp`. Remembered and applied when the sink appears.
    SetVolume {
        sink: SinkRef,
        volume: f32,
        ramp: Duration,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    SinkAppeared(SinkInfo),
    SinkRemoved(SinkInfo),
    Linked(SinkInfo),
    Unlinked(SinkInfo),
}

pub struct AudioEngine {
    commands: pw::channel::Sender<AudioCommand>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Start the PipeWire thread and begin (unlinked, so silent) looped playback of `pcm`.
    pub fn start(pcm: Pcm) -> Result<(Self, UnboundedReceiver<AudioEvent>)> {
        let (commands, rx) = pw::channel::channel();
        let (events, events_rx) = unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || run(pcm, rx, events, ready_tx))
            .context("spawning pipewire thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("pipewire thread died during startup"))??;
        Ok((
            Self {
                commands,
                thread: Some(thread),
            },
            events_rx,
        ))
    }

    pub fn send(&self, cmd: AudioCommand) -> Result<()> {
        self.commands
            .send(cmd)
            .map_err(|_| anyhow!("audio thread is gone"))
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.commands.send(AudioCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    pcm: Pcm,
    rx: pw::channel::Receiver<AudioCommand>,
    events: UnboundedSender<AudioEvent>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) {
    pw::init();
    let setup = || -> Result<_> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating main loop")?;
        let context = ContextRc::new(&mainloop, None).context("creating context")?;
        let core = context
            .connect_rc(None)
            .context("connecting to PipeWire (is it running?)")?;
        let registry = core.get_registry_rc().context("getting registry")?;
        let playback = stream::start(&core, &pcm)?;
        Ok((mainloop, context, core, registry, playback))
    };
    let (mainloop, _context, core, registry, _playback) = match setup() {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let engine = Rc::new(RefCell::new(Engine {
        core,
        registry: registry.clone(),
        graph: Graph::default(),
        wanted: HashSet::new(),
        links: HashMap::new(),
        nodes: HashMap::new(),
        devices: HashMap::new(),
        volumes: HashMap::new(),
        current: HashMap::new(),
        ramps: HashMap::new(),
        reasserts: HashMap::new(),
        events,
    }));

    let (e1, e2) = (engine.clone(), engine.clone());
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| e1.borrow_mut().on_global(g))
        .global_remove(move |id| e2.borrow_mut().on_remove(id))
        .register();

    let (e3, ml) = (engine.clone(), mainloop.clone());
    let _commands = rx.attach(mainloop.loop_(), move |cmd| match cmd {
        AudioCommand::Shutdown => ml.quit(),
        cmd => e3.borrow_mut().command(cmd),
    });

    let e4 = engine.clone();
    let timer = mainloop
        .loop_()
        .add_timer(move |_| e4.borrow_mut().tick(Instant::now()));
    timer.update_timer(Some(TICK), Some(TICK));

    let _ = ready.send(Ok(()));
    mainloop.run();
    // Dropping `engine` (links) and the stream tears everything down.
    tracing::debug!("audio thread exiting");
}

struct DeviceProxy {
    device: Device,
    _listener: DeviceListener,
    route: Rc<Cell<Option<Route>>>,
}

struct Engine {
    core: CoreRc,
    registry: RegistryRc,
    graph: Graph,
    wanted: HashSet<SinkRef>,
    /// sink node id -> its FL/FR links. Dropping a `Link` removes it from the graph.
    links: HashMap<u32, Vec<Link>>,
    /// Bound proxies of sink nodes, used to write `Props`.
    nodes: HashMap<u32, Node>,
    /// Bluetooth devices by device id. Their output route carries the volume WirePlumber
    /// restores and `wpctl` shows; it is a separate stage from the node `Props`.
    devices: HashMap<u32, DeviceProxy>,
    /// Requested volume per sink, applied when (re)appearing.
    volumes: HashMap<SinkRef, (f32, Duration)>,
    /// Last perceptual volume written per sink node.
    current: HashMap<u32, f32>,
    ramps: HashMap<u32, Ramp>,
    reasserts: HashMap<u32, Instant>,
    events: UnboundedSender<AudioEvent>,
}

impl Engine {
    fn emit(&self, ev: AudioEvent) {
        let _ = self.events.send(ev);
    }

    fn on_global(&mut self, global: &GlobalObject<&DictRef>) {
        let Some(props) = global.props else { return };
        match global.type_ {
            ObjectType::Node => {
                if let Some(info) = self.graph.add_node(global.id, |k| props.get(k)) {
                    match self.registry.bind::<Node, _>(global) {
                        Ok(node) => {
                            self.nodes.insert(global.id, node);
                        }
                        Err(e) => tracing::warn!("cannot bind sink {}: {e}", info.name),
                    }
                    tracing::debug!("sink appeared: {} ({})", info.name, global.id);
                    self.emit(AudioEvent::SinkAppeared(info.clone()));
                    let wanted = self
                        .volumes
                        .iter()
                        .find(|(r, _)| info.matches(r))
                        .map(|(_, &v)| v);
                    if let Some((volume, ramp)) = wanted {
                        self.apply_volume(info.node_id, volume, ramp, Instant::now());
                    }
                }
            }
            ObjectType::Device if props.get("device.api") == Some("bluez5") => {
                match self.registry.bind::<Device, _>(global) {
                    Ok(device) => {
                        let route = Rc::new(Cell::new(None));
                        let r = route.clone();
                        let listener = device
                            .add_listener_local()
                            .param(move |_, _, _, _, pod| {
                                if let Some(found) = pod.and_then(volume::parse_output_route) {
                                    r.set(Some(found));
                                }
                            })
                            .register();
                        device.subscribe_params(&[ParamType::Route]);
                        self.devices.insert(
                            global.id,
                            DeviceProxy {
                                device,
                                _listener: listener,
                                route,
                            },
                        );
                    }
                    Err(e) => tracing::warn!("cannot bind device {}: {e}", global.id),
                }
            }
            ObjectType::Port => self.graph.add_port(global.id, |k| props.get(k)),
            _ => return,
        }
        self.reconcile();
    }

    fn on_remove(&mut self, id: u32) {
        if let Removed::Sink(info) = self.graph.remove(id) {
            if self.links.remove(&id).is_some() {
                self.emit(AudioEvent::Unlinked(info.clone()));
            }
            self.nodes.remove(&id);
            self.current.remove(&id);
            self.ramps.remove(&id);
            self.reasserts.remove(&id);
            tracing::debug!("sink removed: {} ({id})", info.name);
            self.emit(AudioEvent::SinkRemoved(info));
        }
    }

    fn command(&mut self, cmd: AudioCommand) {
        match cmd {
            AudioCommand::Link(r) => {
                self.wanted.insert(r);
                self.reconcile();
            }
            AudioCommand::Unlink(r) => {
                self.wanted.remove(&r);
                if let Some(info) = self.graph.find(&r).cloned()
                    && self.links.remove(&info.node_id).is_some()
                {
                    self.emit(AudioEvent::Unlinked(info));
                }
            }
            AudioCommand::SetVolume { sink, volume, ramp } => {
                if let Some(id) = self.graph.find(&sink).map(|s| s.node_id) {
                    self.apply_volume(id, volume, ramp, Instant::now());
                }
                self.volumes.insert(sink, (volume, ramp));
            }
            AudioCommand::Shutdown => {}
        }
    }

    /// Create any missing links for wanted sinks whose ports are all present.
    fn reconcile(&mut self) {
        let todo: Vec<(SinkInfo, _)> = self
            .wanted
            .iter()
            .filter_map(|r| self.graph.find(r))
            .filter(|s| !self.links.contains_key(&s.node_id))
            // If a volume was requested, let it land first so the stream never opens at the
            // speaker's restored volume (audible as a click).
            .filter(|s| {
                self.current.contains_key(&s.node_id) || !self.volumes.keys().any(|r| s.matches(r))
            })
            .filter_map(|s| Some((s.clone(), self.graph.link_specs(s.node_id)?)))
            .collect();
        for (sink, specs) in todo {
            let mut links = Vec::with_capacity(2);
            for spec in specs {
                let created = self.core.create_object::<Link>(
                    "link-factory",
                    &properties! {
                        *keys::LINK_OUTPUT_NODE => spec.output.node.to_string(),
                        *keys::LINK_OUTPUT_PORT => spec.output.port.to_string(),
                        *keys::LINK_INPUT_NODE => spec.input.node.to_string(),
                        *keys::LINK_INPUT_PORT => spec.input.port.to_string(),
                        // Links must die with us, not outlive the process.
                        "object.linger" => "false",
                    },
                );
                match created {
                    Ok(l) => links.push(l),
                    Err(e) => tracing::warn!("linking to {} failed: {e}", sink.name),
                }
            }
            if links.len() == 2 {
                tracing::debug!("linked to {}", sink.name);
                self.links.insert(sink.node_id, links);
                self.emit(AudioEvent::Linked(sink));
            }
        }
    }

    fn apply_volume(&mut self, id: u32, target: f32, ramp: Duration, now: Instant) {
        let from = if ramp.is_zero() {
            target
        } else {
            self.current.get(&id).copied().unwrap_or(0.0)
        };
        self.ramps.insert(
            id,
            Ramp {
                from,
                to: target,
                start: now,
                duration: ramp,
            },
        );
        self.reasserts.insert(id, now + ramp + REASSERT_DELAY);
        self.tick(now);
    }

    /// Write a volume; returns false if the sink is not ready to take it yet.
    fn write_volume(&mut self, id: u32, v: f32) -> bool {
        let device = self
            .graph
            .sink(id)
            .and_then(|s| s.device_id)
            .and_then(|d| self.devices.get(&d));
        let ok = match device {
            // Bluetooth: set the device route (what `wpctl` does) and leave the node at 1.0.
            Some(dev) => match dev.route.get() {
                Some(route) => {
                    let bytes = volume::route_pod(route, v);
                    dev.device
                        .set_param(ParamType::Route, 0, volume::pod_from(&bytes));
                    if !self.current.contains_key(&id) {
                        self.write_node_volume(id, 1.0);
                    }
                    true
                }
                None => false,
            },
            None => self.write_node_volume(id, v),
        };
        if ok {
            self.current.insert(id, v);
        }
        ok
    }

    fn write_node_volume(&self, id: u32, v: f32) -> bool {
        let Some(node) = self.nodes.get(&id) else {
            return false;
        };
        let bytes = volume::props_pod(v);
        node.set_param(ParamType::Props, 0, volume::pod_from(&bytes));
        true
    }

    /// Advance ramps and fire one-shot volume re-assertions.
    fn tick(&mut self, now: Instant) {
        if self.ramps.is_empty() && self.reasserts.is_empty() {
            return;
        }
        let ramps: Vec<(u32, Ramp)> = self.ramps.iter().map(|(&i, &r)| (i, r)).collect();
        for (id, ramp) in ramps {
            let wrote = self.write_volume(id, ramp.value_at(now));
            if wrote && ramp.finished(now) {
                self.ramps.remove(&id);
            }
        }
        self.reconcile();
        let due: Vec<u32> = self
            .reasserts
            .iter()
            .filter(|&(id, &at)| at <= now && !self.ramps.contains_key(id))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            self.reasserts.remove(&id);
            if let Some(v) = self.current.get(&id).copied() {
                tracing::debug!("re-asserting volume {v:.2} on node {id}");
                self.write_volume(id, v);
            }
        }
    }
}
