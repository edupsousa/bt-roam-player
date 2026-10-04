//! Pure bookkeeping of the PipeWire graph: which sink nodes and ports exist, and which
//! port pairs make up a link. No PipeWire calls happen here, so it is unit-testable.
use std::collections::HashMap;

use bluer::Address;

use super::stream::OWN_NODE_NAME;

/// How a caller names a sink: by Bluetooth address (normal use) or PipeWire `node.name`
/// (dev CLI, non-Bluetooth sinks).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SinkRef {
    Address(Address),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInfo {
    pub node_id: u32,
    pub name: String,
    /// From `api.bluez5.address`; `None` for non-Bluetooth sinks.
    pub address: Option<Address>,
    /// Owning PipeWire device (`device.id`).
    pub device_id: Option<u32>,
}

impl SinkInfo {
    pub fn matches(&self, r: &SinkRef) -> bool {
        match r {
            SinkRef::Address(a) => self.address == Some(*a),
            SinkRef::Name(n) => &self.name == n,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSpec {
    pub node: u32,
    pub port: u32,
}

/// One FL/FR pair to link: our output port -> the sink's input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkSpec {
    pub output: PortSpec,
    pub input: PortSpec,
}

#[derive(Debug)]
struct Port {
    node: u32,
    output: bool,
    channel: String,
}

/// Registry node globals carry only a subset of the properties (no `api.bluez5.address`),
/// but the WirePlumber-assigned name embeds the address: `bluez_output.00_11_22_33_44_55.1`.
fn address_from_node_name(name: &str) -> Option<Address> {
    let mac = name.strip_prefix("bluez_output.")?.get(..17)?;
    mac.replace('_', ":").parse().ok()
}

#[derive(Debug, Default)]
pub struct Graph {
    sinks: HashMap<u32, SinkInfo>,
    ports: HashMap<u32, Port>,
    own_node: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Removed {
    Sink(SinkInfo),
    Other,
}

impl Graph {
    /// Register a node global. Returns the sink info when it is an audio sink.
    pub fn add_node<'a>(
        &mut self,
        id: u32,
        get: impl Fn(&str) -> Option<&'a str>,
    ) -> Option<SinkInfo> {
        let name = get("node.name")?;
        if name == OWN_NODE_NAME {
            self.own_node = Some(id);
            return None;
        }
        if get("media.class") != Some("Audio/Sink") {
            return None;
        }
        let info = SinkInfo {
            node_id: id,
            name: name.to_string(),
            address: get("api.bluez5.address")
                .and_then(|a| a.parse().ok())
                .or_else(|| address_from_node_name(name)),
            device_id: get("device.id").and_then(|d| d.parse().ok()),
        };
        self.sinks.insert(id, info.clone());
        Some(info)
    }

    pub fn add_port<'a>(&mut self, id: u32, get: impl Fn(&str) -> Option<&'a str>) {
        let (Some(node), Some(dir), Some(channel)) = (
            get("node.id").and_then(|n| n.parse().ok()),
            get("port.direction"),
            get("audio.channel"),
        ) else {
            return;
        };
        self.ports.insert(
            id,
            Port {
                node,
                output: dir == "out",
                channel: channel.to_string(),
            },
        );
    }

    pub fn remove(&mut self, id: u32) -> Removed {
        self.ports.remove(&id);
        if self.own_node == Some(id) {
            self.own_node = None;
        }
        match self.sinks.remove(&id) {
            Some(info) => Removed::Sink(info),
            None => Removed::Other,
        }
    }

    pub fn sink(&self, node_id: u32) -> Option<&SinkInfo> {
        self.sinks.get(&node_id)
    }

    pub fn find(&self, r: &SinkRef) -> Option<&SinkInfo> {
        self.sinks.values().find(|s| s.matches(r))
    }

    fn port(&self, node: u32, output: bool, channel: &str) -> Option<PortSpec> {
        self.ports
            .iter()
            .find(|(_, p)| p.node == node && p.output == output && p.channel == channel)
            .map(|(&port, _)| PortSpec { node, port })
    }

    /// The FL and FR links needed to connect us to `sink`, once all four ports exist.
    pub fn link_specs(&self, sink: u32) -> Option<[LinkSpec; 2]> {
        let own = self.own_node?;
        let pair = |ch| {
            Some(LinkSpec {
                output: self.port(own, true, ch)?,
                input: self.port(sink, false, ch)?,
            })
        };
        Some([pair("FL")?, pair("FR")?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Props = HashMap<&'static str, &'static str>;

    fn node(g: &mut Graph, id: u32, props: &Props) -> Option<SinkInfo> {
        g.add_node(id, |k| props.get(k).copied())
    }

    fn port(g: &mut Graph, id: u32, node: &'static str, dir: &'static str, ch: &'static str) {
        let p: Props = [
            ("node.id", node),
            ("port.direction", dir),
            ("audio.channel", ch),
        ]
        .into();
        g.add_port(id, |k| p.get(k).copied());
    }

    fn bt_sink() -> Props {
        [
            ("node.name", "bluez_output.00_11_22_33_44_55.1"),
            ("media.class", "Audio/Sink"),
            ("api.bluez5.address", "00:11:22:33:44:55"),
        ]
        .into()
    }

    #[test]
    fn recognises_bluetooth_sink() {
        let mut g = Graph::default();
        let info = node(&mut g, 90, &bt_sink()).unwrap();
        let addr: Address = "00:11:22:33:44:55".parse().unwrap();
        assert_eq!(info.address, Some(addr));
        assert!(g.find(&SinkRef::Address(addr)).is_some());
        assert!(
            g.find(&SinkRef::Name("bluez_output.00_11_22_33_44_55.1".into()))
                .is_some()
        );
    }

    #[test]
    fn address_parsed_from_node_name() {
        let mut g = Graph::default();
        let p: Props = [
            ("node.name", "bluez_output.00_11_22_33_44_55.1"),
            ("media.class", "Audio/Sink"),
        ]
        .into();
        let addr: Address = "00:11:22:33:44:55".parse().unwrap();
        assert_eq!(node(&mut g, 90, &p).unwrap().address, Some(addr));
    }

    #[test]
    fn ignores_non_sinks() {
        let mut g = Graph::default();
        let p: Props = [
            ("node.name", "firefox"),
            ("media.class", "Stream/Output/Audio"),
        ]
        .into();
        assert!(node(&mut g, 5, &p).is_none());
    }

    #[test]
    fn links_need_all_four_ports() {
        let mut g = Graph::default();
        node(&mut g, 7, &[("node.name", OWN_NODE_NAME)].into());
        node(&mut g, 90, &bt_sink());
        port(&mut g, 70, "7", "out", "FL");
        port(&mut g, 71, "7", "out", "FR");
        port(&mut g, 900, "90", "in", "FL");
        assert!(g.link_specs(90).is_none());
        port(&mut g, 901, "90", "in", "FR");
        let [fl, fr] = g.link_specs(90).unwrap();
        assert_eq!((fl.output.port, fl.input.port), (70, 900));
        assert_eq!((fr.output.port, fr.input.port), (71, 901));
        // A monitor/other-direction port must not be mistaken for an input.
        port(&mut g, 902, "90", "out", "FL");
        assert_eq!(g.link_specs(90).unwrap()[0].input.port, 900);
    }

    #[test]
    fn removal_reports_sink() {
        let mut g = Graph::default();
        node(&mut g, 90, &bt_sink());
        assert!(matches!(g.remove(90), Removed::Sink(s) if s.node_id == 90));
        assert_eq!(g.remove(90), Removed::Other);
    }
}
