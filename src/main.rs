mod config;

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
        Command::List => bail!("`list` is not implemented yet (M4)"),
        Command::Forget { address } => {
            bail!("`forget` is not implemented yet (M5); address: {address}")
        }
    }
}
