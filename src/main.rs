mod audio;
mod config;
mod proximity;

use std::path::PathBuf;

use anyhow::{Result, bail};
use bluer::Address;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Play one looping audio file to every nearby Bluetooth speaker"
)]
struct Cli {
    /// Path to config.toml (default: ./config.toml, then ~/.config/bt-roam-player/config.toml)
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Increase log verbosity (-v debug, -vv trace); overridden by RUST_LOG
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Play a looping file to every speaker that comes into range
    Run {
        /// Audio file to loop (flac, mp3, ogg, wav)
        #[arg(short, long, value_name = "PATH")]
        file: PathBuf,
    },
    /// Dev tool (M2): loop a file to chosen PipeWire sinks, without any Bluetooth logic
    Play {
        /// Audio file to loop (flac, mp3, ogg, wav)
        #[arg(short, long, value_name = "PATH")]
        file: PathBuf,
        /// PipeWire node.name of a sink to link (repeatable)
        #[arg(short, long = "sink", value_name = "NODE_NAME")]
        sinks: Vec<String>,
        /// Bluetooth address of a sink to link (repeatable)
        #[arg(short, long = "address", value_name = "ADDR")]
        addresses: Vec<Address>,
        /// Volume (0.0-1.0) to set on each linked sink; omit to leave it alone
        #[arg(long)]
        volume: Option<f32>,
        /// Seconds to ramp the volume up from 0
        #[arg(long, default_value_t = 0.5)]
        ramp: f32,
        /// Unlink every sink after this many seconds (keeps playing, silent)
        #[arg(long)]
        unlink_after: Option<u64>,
        /// Stop after this many seconds (default: until Ctrl-C)
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Show the audio speakers currently seen, with RSSI and paired/connected status
    List,
    /// Remove a paired speaker
    Forget {
        /// Bluetooth address, e.g. AA:BB:CC:DD:EE:FF
        address: Address,
    },
}

fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let config = Config::load(cli.config.as_deref())?;
    tracing::debug!(?config, "configuration loaded");

    match cli.command {
        Command::Run { file } => bail!(
            "`run` is not implemented yet (M6); file: {}",
            file.display()
        ),
        Command::Play {
            file,
            sinks,
            addresses,
            volume,
            ramp,
            unlink_after,
            seconds,
        } => play(file, sinks, addresses, volume, ramp, unlink_after, seconds).await,
        Command::List => bail!("`list` is not implemented yet (M4)"),
        Command::Forget { address } => {
            bail!("`forget` is not implemented yet (M5); address: {address}")
        }
    }
}

async fn play(
    file: PathBuf,
    sinks: Vec<String>,
    addresses: Vec<Address>,
    volume: Option<f32>,
    ramp: f32,
    unlink_after: Option<u64>,
    seconds: Option<u64>,
) -> Result<()> {
    use std::time::Duration;

    use crate::audio::{AudioCommand, AudioEngine, SinkRef, decode_file};

    let pcm = decode_file(&file)?;
    tracing::info!(
        "decoded {}: {:.1}s at {} Hz",
        file.display(),
        pcm.frames() as f32 / pcm.rate as f32,
        pcm.rate
    );
    let (engine, mut events) = AudioEngine::start(pcm)?;
    let refs: Vec<SinkRef> = sinks
        .into_iter()
        .map(SinkRef::Name)
        .chain(addresses.into_iter().map(SinkRef::Address))
        .collect();
    for sink in refs.iter().cloned() {
        if let Some(volume) = volume {
            engine.send(AudioCommand::SetVolume {
                sink: sink.clone(),
                volume,
                ramp: Duration::from_secs_f32(ramp),
            })?;
        }
        engine.send(AudioCommand::Link(sink))?;
    }
    let unlink = async {
        match unlink_after {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(unlink);
    let mut unlinked = false;
    let deadline = async {
        match seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut deadline => break,
            _ = &mut unlink, if !unlinked => {
                unlinked = true;
                for sink in refs.iter().cloned() {
                    engine.send(AudioCommand::Unlink(sink))?;
                }
            }
            Some(ev) = events.recv() => tracing::info!("{ev:?}"),
        }
    }
    Ok(())
}
