//! M1 spike (throwaway): PipeWire fan-out.
//!
//!   spike_pw <secs> <sink node.name> [<sink node.name> ...]
//!
//! Plays a 440 Hz tone from a stream created with autoconnect=false and links its FL/FR
//! output ports to every named sink (link-factory, object.linger=false).
use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

use pipewire as pw;
use pw::{properties::properties, spa, types::ObjectType};
use spa::pod::Pod;

const RATE: u32 = 48000;

#[derive(Default)]
struct Graph {
    /// node.name -> node id
    nodes: HashMap<String, u32>,
    /// (node id, direction, channel) -> port id
    ports: HashMap<(u32, String, String), u32>,
}

fn main() -> Result<(), pw::Error> {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args[1].parse().unwrap();
    let sinks: Vec<String> = args[2..].to_vec();

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let graph = Rc::new(RefCell::new(Graph::default()));
    let g = graph.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            let mut g = g.borrow_mut();
            match global.type_ {
                ObjectType::Node => {
                    if let Some(name) = props.get("node.name") {
                        g.nodes.insert(name.to_string(), global.id);
                    }
                }
                ObjectType::Port => {
                    if let (Some(n), Some(d), Some(c)) = (
                        props.get("node.id"),
                        props.get("port.direction"),
                        props.get("audio.channel"),
                    ) {
                        g.ports.insert(
                            (n.parse().unwrap(), d.to_string(), c.to_string()),
                            global.id,
                        );
                    }
                }
                _ => {}
            }
        })
        .register();

    let stream = pw::stream::StreamBox::new(
        &core,
        "spike-player",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => "spike-player",
            *pw::keys::NODE_AUTOCONNECT => "false",
            "node.dont-reconnect" => "true",
        },
    )?;

    let _stream_listener = stream
        .add_local_listener_with_user_data(0.0f64)
        .state_changed(|_, _, old, new| eprintln!("stream state {old:?} -> {new:?}"))
        .process(|stream, phase| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let data = &mut buffer.datas_mut()[0];
            let stride = 8; // 2ch * f32
            let n = if let Some(slice) = data.data() {
                let n = slice.len() / stride;
                for i in 0..n {
                    *phase += std::f64::consts::TAU * 440.0 / RATE as f64;
                    if *phase >= std::f64::consts::TAU {
                        *phase -= std::f64::consts::TAU;
                    }
                    let v = (phase.sin() * 0.2) as f32;
                    slice[i * stride..i * stride + 4].copy_from_slice(&v.to_le_bytes());
                    slice[i * stride + 4..i * stride + 8].copy_from_slice(&v.to_le_bytes());
                }
                n
            } else {
                0
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = (stride * n) as _;
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = libspa_sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = libspa_sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: libspa_sys::SPA_TYPE_OBJECT_Format,
            id: libspa_sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];
    // No AUTOCONNECT: WirePlumber must not route us anywhere.
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // Poll the graph once a second and create any missing links.
    let linked: Rc<RefCell<HashMap<String, Vec<pw::link::Link>>>> = Rc::default();
    let (core2, graph2, linked2, stream_ref) =
        (core.clone(), graph.clone(), linked.clone(), &stream);
    let own = stream_ref.node_id();
    let _ = own;
    let timer = mainloop.loop_().add_timer({
        let stream_node = Rc::new(RefCell::new(None::<u32>));
        let sn = stream_node.clone();
        let graph3 = graph2.clone();
        move |_| {
            let g = graph3.borrow();
            let Some(&me) = g.nodes.get("spike-player") else {
                return;
            };
            *sn.borrow_mut() = Some(me);
            for sink in &sinks {
                if linked2.borrow().contains_key(sink) {
                    continue;
                }
                let Some(&node) = g.nodes.get(sink) else {
                    eprintln!("sink {sink} not in graph yet");
                    continue;
                };
                let mut links = Vec::new();
                for ch in ["FL", "FR"] {
                    let out = g.ports.get(&(me, "out".into(), ch.into()));
                    let inp = g.ports.get(&(node, "in".into(), ch.into()));
                    let (Some(out), Some(inp)) = (out, inp) else {
                        eprintln!("ports for {sink}/{ch} missing (out={out:?} in={inp:?})");
                        continue;
                    };
                    match core2.create_object::<pw::link::Link>(
                        "link-factory",
                        &properties! {
                            "link.output.node" => me.to_string(),
                            "link.output.port" => out.to_string(),
                            "link.input.node" => node.to_string(),
                            "link.input.port" => inp.to_string(),
                            "object.linger" => "false",
                        },
                    ) {
                        Ok(l) => links.push(l),
                        Err(e) => eprintln!("link {sink}/{ch} failed: {e}"),
                    }
                }
                if links.len() == 2 {
                    eprintln!("linked spike-player -> {sink}");
                    linked2.borrow_mut().insert(sink.clone(), links);
                }
            }
        }
    });
    timer.update_timer(
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(1)),
    );

    let ml = mainloop.clone();
    let quit = mainloop.loop_().add_timer(move |_| ml.quit());
    quit.update_timer(Some(Duration::from_secs(secs)), None);

    mainloop.run();
    eprintln!("exiting");
    Ok(())
}
