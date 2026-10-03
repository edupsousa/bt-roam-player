//! Per-sink volume: perceptual-to-linear mapping, ramps, and the `Props` pod.
use std::time::{Duration, Instant};

use pipewire::spa::{
    pod::{
        Object, Pod, Property, PropertyFlags, Value, ValueArray, deserialize::PodDeserializer,
        serialize::PodSerializer,
    },
    utils::SpaTypes,
};

/// Map a perceptual volume (what `wpctl`/GNOME show, 0.0..=1.0) to the linear
/// `channelVolumes` factor. PipeWire's own UIs use a cubic curve.
pub fn to_linear(perceptual: f32) -> f32 {
    perceptual.clamp(0.0, 1.0).powi(3)
}

fn props_value(perceptual: f32) -> Value {
    let v = to_linear(perceptual);
    Value::Object(Object {
        type_: SpaTypes::ObjectParamProps.as_raw(),
        id: libspa_sys::SPA_PARAM_Props,
        properties: vec![Property {
            key: libspa_sys::SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vec![v, v])),
        }],
    })
}

/// Serialized `Props { channelVolumes: [v, v] }` for a stereo sink node.
pub fn props_pod(perceptual: f32) -> Vec<u8> {
    serialize(&props_value(perceptual))
}

/// A device output route as announced by the device's `Route` params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub index: i32,
    pub device: i32,
}

/// Extract the output route from a `Route` param (input routes are ignored).
pub fn parse_output_route(pod: &Pod) -> Option<Route> {
    let (_, Value::Object(obj)) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?
    else {
        return None;
    };
    let (mut index, mut device, mut output) = (None, None, false);
    for p in &obj.properties {
        match (p.key, &p.value) {
            (libspa_sys::SPA_PARAM_ROUTE_index, Value::Int(v)) => index = Some(*v),
            (libspa_sys::SPA_PARAM_ROUTE_device, Value::Int(v)) => device = Some(*v),
            (libspa_sys::SPA_PARAM_ROUTE_direction, Value::Id(id)) => {
                output = id.0 == libspa_sys::SPA_DIRECTION_OUTPUT
            }
            _ => {}
        }
    }
    output.then_some(Route {
        index: index?,
        device: device?,
    })
}

/// Serialized `Route` param setting the volume of `route` (not saved by WirePlumber).
/// This is what `wpctl set-volume` writes; the node `Props` volume is a separate,
/// multiplicative stage, so for Bluetooth sinks the route is the one to set.
pub fn route_pod(route: Route, perceptual: f32) -> Vec<u8> {
    serialize(&Value::Object(Object {
        type_: SpaTypes::ObjectParamRoute.as_raw(),
        id: libspa_sys::SPA_PARAM_Route,
        properties: vec![
            Property::new(libspa_sys::SPA_PARAM_ROUTE_index, Value::Int(route.index)),
            Property::new(libspa_sys::SPA_PARAM_ROUTE_device, Value::Int(route.device)),
            Property::new(libspa_sys::SPA_PARAM_ROUTE_props, props_value(perceptual)),
            Property::new(libspa_sys::SPA_PARAM_ROUTE_save, Value::Bool(false)),
        ],
    }))
}

fn serialize(value: &Value) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .expect("serializing a pod cannot fail")
        .0
        .into_inner()
}

pub fn pod_from(bytes: &[u8]) -> &Pod {
    Pod::from_bytes(bytes).expect("pod we just serialized is valid")
}

/// A linear ramp between two perceptual volumes.
#[derive(Debug, Clone, Copy)]
pub struct Ramp {
    pub from: f32,
    pub to: f32,
    pub start: Instant,
    pub duration: Duration,
}

impl Ramp {
    pub fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        self.from + (self.to - self.from) * t
    }

    pub fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_mapping() {
        assert_eq!(to_linear(0.0), 0.0);
        assert_eq!(to_linear(1.0), 1.0);
        assert!((to_linear(0.5) - 0.125).abs() < 1e-6);
        assert_eq!(to_linear(2.0), 1.0);
        assert_eq!(to_linear(-1.0), 0.0);
    }

    #[test]
    fn ramp_interpolates_and_clamps() {
        let start = Instant::now();
        let r = Ramp {
            from: 0.0,
            to: 0.6,
            start,
            duration: Duration::from_millis(500),
        };
        assert_eq!(r.value_at(start), 0.0);
        assert!((r.value_at(start + Duration::from_millis(250)) - 0.3).abs() < 1e-6);
        assert_eq!(r.value_at(start + Duration::from_secs(5)), 0.6);
        assert!(!r.finished(start + Duration::from_millis(499)));
        assert!(r.finished(start + Duration::from_millis(500)));
    }

    #[test]
    fn zero_duration_jumps() {
        let start = Instant::now();
        let r = Ramp {
            from: 0.0,
            to: 0.4,
            start,
            duration: Duration::ZERO,
        };
        assert_eq!(r.value_at(start), 0.4);
    }

    #[test]
    fn route_pod_roundtrips() {
        let bytes = route_pod(
            Route {
                index: 1,
                device: 1,
            },
            0.5,
        );
        let pod = Pod::from_bytes(&bytes).unwrap();
        // A set-volume route has no direction, so it is not mistaken for an output route.
        assert_eq!(parse_output_route(pod), None);
    }

    #[test]
    fn props_pod_parses() {
        let bytes = props_pod(0.5);
        assert!(Pod::from_bytes(&bytes).is_some());
    }
}
