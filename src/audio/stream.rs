//! The single looping playback stream. Created unconnected to any sink
//! (`node.autoconnect=false`, no AUTOCONNECT flag), so WirePlumber never routes it;
//! `graph` links it to speakers explicitly.
use std::{io::Cursor, sync::Arc};

use anyhow::{Context, Result};
use pipewire as pw;
use pw::{
    core::CoreRc,
    properties::properties,
    spa::{
        self,
        param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS},
        pod::{Object, Pod, Value, serialize::PodSerializer},
    },
    stream::{StreamFlags, StreamListener, StreamRc},
};

use super::decode::Pcm;

pub const OWN_NODE_NAME: &str = "bt-roam-player";
const FRAME_BYTES: usize = 2 * size_of::<f32>();

struct Playback {
    samples: Arc<[f32]>,
    /// Next sample index (always even: whole stereo frames).
    pos: usize,
}

impl Playback {
    /// Fill `out` (little-endian f32 stereo) with `frames` frames, wrapping at the end.
    fn fill(&mut self, out: &mut [u8], frames: usize) {
        let mut written = 0;
        while written < frames * 2 {
            let n = (frames * 2 - written).min(self.samples.len() - self.pos);
            for (i, s) in self.samples[self.pos..self.pos + n].iter().enumerate() {
                let at = (written + i) * 4;
                out[at..at + 4].copy_from_slice(&s.to_le_bytes());
            }
            written += n;
            self.pos = (self.pos + n) % self.samples.len();
        }
    }
}

pub struct PlaybackStream {
    _stream: StreamRc,
    _listener: StreamListener<Playback>,
}

pub fn start(core: &CoreRc, pcm: &Pcm) -> Result<PlaybackStream> {
    let stream = StreamRc::new(
        core.clone(),
        OWN_NODE_NAME,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::NODE_NAME => OWN_NODE_NAME,
            *pw::keys::NODE_AUTOCONNECT => "false",
            "node.dont-reconnect" => "true",
        },
    )
    .context("creating stream")?;

    let state = Playback {
        samples: pcm.samples.clone().into(),
        pos: 0,
    };
    let listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, _, old, new| tracing::debug!("stream state {old:?} -> {new:?}"))
        .process(|stream, playback| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let data = &mut buffer.datas_mut()[0];
            let frames = match data.data() {
                Some(slice) => {
                    let frames = slice.len() / FRAME_BYTES;
                    playback.fill(slice, frames);
                    frames
                }
                None => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = FRAME_BYTES as _;
            *chunk.size_mut() = (frames * FRAME_BYTES) as _;
        })
        .register()
        .context("registering stream listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(pcm.rate);
    info.set_channels(2);
    let mut position = [0; MAX_CHANNELS];
    position[0] = libspa_sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = libspa_sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let bytes = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: libspa_sys::SPA_TYPE_OBJECT_Format,
            id: libspa_sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .context("serializing format")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).context("format pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .context("connecting stream")?;

    Ok(PlaybackStream {
        _stream: stream,
        _listener: listener,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(out: &[u8]) -> Vec<f32> {
        out.as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()
    }

    #[test]
    fn fill_wraps_without_gap() {
        let mut p = Playback {
            samples: vec![1., 2., 3., 4., 5., 6.].into(),
            pos: 0,
        };
        let mut out = [0u8; 16];
        p.fill(&mut out, 2);
        assert_eq!(read(&out), [1., 2., 3., 4.]);
        p.fill(&mut out, 2);
        assert_eq!(read(&out), [5., 6., 1., 2.]);
        p.fill(&mut out, 2);
        assert_eq!(read(&out), [3., 4., 5., 6.]);
    }
}
