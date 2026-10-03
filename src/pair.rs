//! `pair` and `forget`: dev commands around the pairing agent.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use bluer::Address;

use crate::bluetooth::adapter::{Bluetooth, BtEvent};
use crate::bluetooth::candidate::evaluate;
use crate::bluetooth::device::DeviceSnapshot;
use crate::bluetooth::pairing::{PairError, PairOutcome, Pairing};
use crate::config::Config;

/// Don't retry a device that failed to pair sooner than this.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Run the agent and pair every unpaired audio candidate that shows up, for `seconds`.
pub async fn pair(config: &Config, seconds: u64) -> Result<()> {
    let (bt, mut events) = Bluetooth::start(&config.discovery).await?;
    let pairing = Pairing::start(bt.session(), bt.adapter().clone(), config).await?;
    tracing::info!("agent registered; put a speaker in pairing mode ({seconds} s)");
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut attempted: HashMap<Address, Instant> = HashMap::new();
    let mut ignored: Vec<Address> = Vec::new();
    let mut known: HashMap<Address, DeviceSnapshot> = HashMap::new();
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
            Some(event) = events.recv() => {
                let s = match event {
                    BtEvent::Updated(s) => { known.insert(s.address, s.clone()); s }
                    BtEvent::Rssi { address, dbm } => {
                        let Some(s) = known.get_mut(&address) else { continue };
                        s.rssi = Some(dbm);
                        s.clone()
                    }
                    BtEvent::Removed(address) => { known.remove(&address); continue }
                };
                if s.paired || s.rssi.is_none() {
                    continue;
                }
                let verdict = evaluate(&s.info(), &config.allow, &config.deny);
                if !verdict.is_accepted() {
                    if !ignored.contains(&s.address) {
                        ignored.push(s.address);
                        tracing::info!(address = %s.address, name = ?s.name, "ignoring: {verdict:?}");
                    }
                    continue;
                }
                if attempted.get(&s.address).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
                    continue;
                }
                attempted.insert(s.address, Instant::now());
                tracing::info!(address = %s.address, name = ?s.name, "pairing");
                match pairing.ensure_paired(s.address).await {
                    Ok(PairOutcome::Paired) => {
                        tracing::info!(address = %s.address, "paired, trusted and bonded")
                    }
                    Ok(PairOutcome::AlreadyBonded) => {
                        tracing::info!(address = %s.address, "already bonded")
                    }
                    Err(PairError::NotCandidate(v)) => tracing::info!("skipped: {v:?}"),
                    Err(e) => tracing::warn!(address = %s.address, "pairing failed: {e}"),
                }
            }
        }
    }
    Ok(())
}

pub async fn forget(config: &Config, address: Address) -> Result<()> {
    let (bt, _events) = Bluetooth::start(&config.discovery).await?;
    let pairing = Pairing::start(bt.session(), bt.adapter().clone(), config).await?;
    pairing.forget(address).await?;
    println!("removed {address}");
    Ok(())
}
