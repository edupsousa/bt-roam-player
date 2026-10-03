//! `list`: dev tool that shows the audio candidates BlueZ currently knows about, with raw
//! and smoothed RSSI, for a few seconds of discovery.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use bluer::Address;

use crate::bluetooth::adapter::{Bluetooth, BtEvent};
use crate::bluetooth::candidate::{Verdict, evaluate};
use crate::bluetooth::device::DeviceSnapshot;
use crate::bluetooth::mgmt::MgmtError;
use crate::bluetooth::rssi::{MgmtRssi, RssiSource};
use crate::config::Config;
use crate::proximity::{Params, State, Tracker};

struct Row {
    snapshot: DeviceSnapshot,
    verdict: Verdict,
    /// Discovery RSSI, smoothed.
    dbm: Tracker,
    /// Link RSSI (relative), smoothed; only for connected devices.
    link: Tracker,
    link_raw: Option<f64>,
}

pub async fn list(config: &Config, seconds: u64, all: bool) -> Result<()> {
    let (bt, mut events) = Bluetooth::start(&config.discovery).await?;
    let mgmt = match MgmtRssi::new(bt.index()) {
        Ok(m) => Some(m),
        Err(MgmtError::PermissionDenied) => {
            tracing::warn!(
                "no CAP_NET_ADMIN: link RSSI of connected speakers is unavailable \
                 (setcap cap_net_admin+ep on the binary, or run it with sudo)"
            );
            None
        }
        Err(e) => {
            tracing::warn!("management socket: {e}");
            None
        }
    };
    let (dbm_params, link_params) = (
        Params::dbm(&config.proximity),
        Params::mgmt(&config.proximity),
    );
    let start = Instant::now();
    let mut rows: BTreeMap<Address, Row> = BTreeMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = poll.tick() => {
                let Some(mgmt) = &mgmt else { continue };
                for (addr, row) in rows.iter_mut().filter(|(_, r)| r.snapshot.connected) {
                    if let Some(v) = mgmt.read(*addr).await {
                        row.link_raw = Some(v);
                        row.link.sample(start.elapsed(), v);
                    }
                }
            }
            Some(event) = events.recv() => match event {
                BtEvent::Updated(snapshot) => {
                    let verdict = evaluate(&snapshot.info(), &config.allow, &config.deny);
                    let now = start.elapsed();
                    let row = rows.entry(snapshot.address).or_insert_with(|| Row {
                        snapshot: snapshot.clone(),
                        verdict,
                        dbm: Tracker::new(dbm_params, State::Far),
                        link: Tracker::new(link_params, State::Far),
                        link_raw: None,
                    });
                    if row.dbm.smoothed().is_none() && let Some(rssi) = snapshot.rssi {
                        row.dbm.sample(now, f64::from(rssi));
                    }
                    row.verdict = verdict;
                    row.snapshot = snapshot;
                }
                BtEvent::Rssi { address, dbm } => {
                    if let Some(row) = rows.get_mut(&address) {
                        row.snapshot.rssi = Some(dbm);
                        row.dbm.sample(start.elapsed(), f64::from(dbm));
                    }
                }
                BtEvent::Removed(address) => { rows.remove(&address); }
            },
        }
    }

    println!(
        "{:<17}  {:<24} {:<8} {:<6} {:<4} {:<4} {:<5} {:>7} {:>7} {:>7}  verdict",
        "address", "name", "class", "paired", "conn", "trst", "", "dBm", "smooth", "link dB"
    );
    for (addr, row) in rows.iter().filter(|(_, r)| all || r.verdict.is_accepted()) {
        let s = &row.snapshot;
        println!(
            "{addr}  {:<24} {:<8} {:<6} {:<4} {:<4} {:<5} {:>7} {:>7} {:>7}  {:?}",
            s.name
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(24)
                .collect::<String>(),
            s.class.map_or("-".into(), |c| format!("{c:#08x}")),
            yes(s.paired),
            yes(s.connected),
            yes(s.trusted),
            "",
            s.rssi.map_or("-".into(), |v| v.to_string()),
            row.dbm.smoothed().map_or("-".into(), |v| format!("{v:.1}")),
            row.link
                .smoothed()
                .or(row.link_raw)
                .map_or("-".into(), |v| format!("{v:.1}")),
            row.verdict,
        );
    }
    Ok(())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}
