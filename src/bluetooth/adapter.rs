//! The BlueZ adapter: power, pairable, duty-cycled discovery, and a normalised stream of
//! device events.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bluer::{AdapterEvent, Address, DeviceEvent, DeviceProperty};
use futures::{StreamExt, pin_mut};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::device::DeviceSnapshot;
use crate::config::DiscoveryConfig;

#[derive(Debug, Clone, PartialEq)]
pub enum BtEvent {
    /// A device appeared or one of the properties we care about changed.
    Updated(DeviceSnapshot),
    /// New discovery RSSI (dBm). Separate from `Updated` because it is frequent.
    Rssi { address: Address, dbm: i16 },
    /// BlueZ forgot the device (not seen for a while and not paired).
    Removed(Address),
    /// The adapter event stream ended: bluetoothd went away.
    Lost,
}

/// Owns the background tasks; they stop when this is dropped.
pub struct Bluetooth {
    adapter: bluer::Adapter,
    tasks: Vec<JoinHandle<()>>,
    // Keeps the D-Bus connection alive.
    _session: bluer::Session,
}

impl Bluetooth {
    pub async fn start(
        adapter_name: Option<&str>,
        discovery: &DiscoveryConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<BtEvent>)> {
        let session = bluer::Session::new()
            .await
            .context("connecting to bluetoothd")?;
        let adapter = match adapter_name {
            Some(name) => {
                let mut names = session.adapter_names().await?;
                if !names.iter().any(|n| n == name) {
                    names.sort();
                    bail!(
                        "Bluetooth adapter {name} not found (available: {})",
                        names.join(", ")
                    );
                }
                session.adapter(name)?
            }
            None => session
                .default_adapter()
                .await
                .context("no Bluetooth adapter")?,
        };
        adapter.set_powered(true).await?;
        tracing::info!(
            "using Bluetooth adapter {} ({}, \"{}\"){}",
            adapter.name(),
            adapter.address().await?,
            adapter.alias().await?,
            if adapter_name.is_none() {
                " [default]"
            } else {
                ""
            }
        );
        // Without this, pairing appears to succeed but is not bonded (see DESIGN.md).
        adapter.set_pairable(true).await?;
        adapter.set_pairable_timeout(0).await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let events = adapter.events().await?;
        let tasks = vec![
            tokio::spawn(watch_devices(adapter.clone(), events, tx)),
            tokio::spawn(run_discovery(
                adapter.clone(),
                Duration::from_secs_f64(discovery.on_secs),
                Duration::from_secs_f64(discovery.off_secs),
            )),
        ];
        Ok((
            Self {
                adapter,
                tasks,
                _session: session,
            },
            rx,
        ))
    }

    pub fn session(&self) -> &bluer::Session {
        &self._session
    }

    pub fn adapter(&self) -> &bluer::Adapter {
        &self.adapter
    }

    /// Controller index for the management socket (`hci0` is 0).
    pub fn index(&self) -> u16 {
        self.adapter
            .name()
            .strip_prefix("hci")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }
}

impl Drop for Bluetooth {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Scan for `on`, pause for `off`, repeat. With `off == 0` scanning never stops.
async fn run_discovery(adapter: bluer::Adapter, on: Duration, off: Duration) {
    loop {
        match adapter.discover_devices().await {
            Ok(stream) => {
                tracing::debug!("discovery on");
                pin_mut!(stream);
                let drain = async { while stream.next().await.is_some() {} };
                if off.is_zero() {
                    drain.await;
                } else {
                    let _ = tokio::time::timeout(on, drain).await;
                }
                tracing::debug!("discovery off");
            }
            Err(e) => tracing::warn!("starting discovery: {e}"),
        }
        tokio::time::sleep(off.max(Duration::from_secs(1))).await;
    }
}

async fn watch_devices(
    adapter: bluer::Adapter,
    events: impl futures::Stream<Item = AdapterEvent>,
    tx: mpsc::UnboundedSender<BtEvent>,
) {
    pin_mut!(events);
    let mut watchers: HashMap<Address, JoinHandle<()>> = HashMap::new();
    let start = |watchers: &mut HashMap<Address, JoinHandle<()>>, address| {
        watchers
            .entry(address)
            .or_insert_with(|| tokio::spawn(watch_device(adapter.clone(), address, tx.clone())));
    };
    // The event stream was opened before this listing, so no device can slip between.
    match adapter.device_addresses().await {
        Ok(known) => known.into_iter().for_each(|a| start(&mut watchers, a)),
        Err(e) => tracing::warn!("listing known devices: {e}"),
    }
    while let Some(event) = events.next().await {
        match event {
            AdapterEvent::DeviceAdded(address) => start(&mut watchers, address),
            AdapterEvent::DeviceRemoved(address) => {
                if let Some(task) = watchers.remove(&address) {
                    task.abort();
                }
                let _ = tx.send(BtEvent::Removed(address));
            }
            AdapterEvent::PropertyChanged(_) => {}
        }
    }
    tracing::error!("lost the BlueZ event stream (did bluetoothd stop?)");
    let _ = tx.send(BtEvent::Lost);
}

/// Reads of many devices at startup intermittently fail with "Failed to send message" on the
/// D-Bus connection; a device whose first read is lost would stay unknown until it changed.
const READ_RETRIES: u32 = 6;
const READ_RETRY_DELAY: Duration = Duration::from_millis(300);

async fn read_snapshot(device: &bluer::Device) -> Option<DeviceSnapshot> {
    let mut attempts = 0;
    loop {
        match DeviceSnapshot::read(device).await {
            Ok(snapshot) => return Some(snapshot),
            Err(e) if attempts < READ_RETRIES => {
                attempts += 1;
                tracing::debug!(address = %device.address(), "reading device: {e}; retrying");
                tokio::time::sleep(READ_RETRY_DELAY * attempts).await;
            }
            Err(e) => {
                tracing::warn!(address = %device.address(), "reading device failed: {e}");
                return None;
            }
        }
    }
}

async fn watch_device(
    adapter: bluer::Adapter,
    address: Address,
    tx: mpsc::UnboundedSender<BtEvent>,
) {
    let Ok(device) = adapter.device(address) else {
        return;
    };
    let mut attempts = 0;
    let events = loop {
        match device.events().await {
            Ok(events) => break events,
            Err(e) if attempts < READ_RETRIES => {
                attempts += 1;
                tracing::debug!(%address, "subscribing to device events: {e}; retrying");
                tokio::time::sleep(READ_RETRY_DELAY * attempts).await;
            }
            Err(e) => {
                tracing::warn!(%address, "cannot watch device: {e}");
                return;
            }
        }
    };
    pin_mut!(events);
    if let Some(snapshot) = read_snapshot(&device).await {
        let _ = tx.send(BtEvent::Updated(snapshot));
    }
    while let Some(DeviceEvent::PropertyChanged(prop)) = events.next().await {
        match prop {
            DeviceProperty::Rssi(dbm) => {
                let _ = tx.send(BtEvent::Rssi { address, dbm });
            }
            DeviceProperty::Connected(_)
            | DeviceProperty::Paired(_)
            | DeviceProperty::Trusted(_)
            | DeviceProperty::Class(_)
            | DeviceProperty::Uuids(_)
            | DeviceProperty::Name(_)
            | DeviceProperty::Alias(_) => {
                if let Some(snapshot) = read_snapshot(&device).await {
                    let _ = tx.send(BtEvent::Updated(snapshot));
                }
            }
            _ => {}
        }
    }
}
