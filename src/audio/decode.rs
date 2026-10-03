//! Decode a whole audio file to interleaved stereo f32 PCM.
//!
//! The clip is kept at its native sample rate; PipeWire's stream adapter resamples to
//! whatever each sink runs at, so we don't need a resampler of our own.
use std::{fs::File, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::{
    audio::sample::Sample,
    codecs::audio::AudioDecoderOptions,
    errors::Error,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

#[derive(Debug, Clone)]
pub struct Pcm {
    pub rate: u32,
    /// Interleaved L,R samples.
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }
}

pub fn decode_file(path: &Path) -> Result<Pcm> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unrecognised audio format")?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("track has no audio parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .context("unsupported codec")?;

    let mut rate = 0;
    let mut out: Vec<f32> = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    while let Some(packet) = format.next_packet().context("reading packet")? {
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(Error::DecodeError(e)) => {
                tracing::warn!("skipping undecodable packet: {e}");
                continue;
            }
            Err(e) => return Err(e).context("decoding"),
        };
        let spec = buf.spec();
        rate = spec.rate();
        let channels = spec.channels().count();
        tmp.resize(buf.samples_interleaved(), f32::MID);
        buf.copy_to_slice_interleaved(&mut tmp);
        to_stereo(&tmp, channels, &mut out);
    }
    if rate == 0 || out.is_empty() {
        bail!("{} contains no decodable audio", path.display());
    }
    Ok(Pcm { rate, samples: out })
}

/// Append `input` (interleaved, `channels` wide) to `out` as stereo.
/// Mono is duplicated; for more than two channels only the first two are kept.
fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(input.iter().flat_map(|&s| [s, s])),
        2 => out.extend_from_slice(input),
        n => out.extend(input.chunks_exact(n).flat_map(|f| [f[0], f[1]])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_is_duplicated() {
        let mut out = vec![];
        to_stereo(&[0.1, 0.2], 1, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);
    }

    #[test]
    fn extra_channels_dropped() {
        let mut out = vec![];
        to_stereo(&[1., 2., 3., 4., 5., 6.], 3, &mut out);
        assert_eq!(out, [1., 2., 4., 5.]);
    }

    #[test]
    fn decodes_wav() {
        // 4 frames of 8 kHz mono 16-bit PCM.
        let mut wav = Vec::new();
        let data: [i16; 4] = [0, 16384, -16384, 0];
        wav.extend(b"RIFF");
        wav.extend((36 + 8u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(8000u32.to_le_bytes());
        wav.extend(16000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(8u32.to_le_bytes());
        for s in data {
            wav.extend(s.to_le_bytes());
        }
        let dir = std::env::temp_dir().join(format!("btrp-{}.wav", std::process::id()));
        std::fs::write(&dir, wav).unwrap();
        let pcm = decode_file(&dir).unwrap();
        std::fs::remove_file(&dir).ok();
        assert_eq!(pcm.rate, 8000);
        assert_eq!(pcm.frames(), 4);
        assert!((pcm.samples[2] - 0.5).abs() < 1e-3);
    }
}
