//! RSSI sources. Discovery RSSI is true dBm but only updates while the device is
//! advertising or answering inquiries; mgmt RSSI is relative (0 = ideal) but works on a
//! live link. See DESIGN.md, decision 2.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bluer::{Adapter, Address};

use super::mgmt::{AddressType, MgmtError, MgmtSocket, has_net_admin};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RssiScale {
    /// True dBm.
    Dbm,
    /// dB below the controller's ideal receive window; 0 is ideal.
    MgmtRelative,
}

pub trait RssiSource {
    fn scale(&self) -> RssiScale;
    /// Latest reading for `address`, or `None` if there is none right now.
    async fn read(&self, address: Address) -> Option<f64>;
}

/// `Device1.RSSI` as maintained by BlueZ during discovery.
pub struct DiscoveryRssi {
    adapter: Adapter,
}

impl DiscoveryRssi {
    pub fn new(adapter: Adapter) -> Self {
        Self { adapter }
    }
}

impl RssiSource for DiscoveryRssi {
    fn scale(&self) -> RssiScale {
        RssiScale::Dbm
    }

    async fn read(&self, address: Address) -> Option<f64> {
        let device = self.adapter.device(address).ok()?;
        device.rssi().await.ok().flatten().map(f64::from)
    }
}

/// RSSI of the live ACL link through the kernel management socket.
pub struct MgmtRssi {
    socket: Arc<Mutex<MgmtSocket>>,
    index: u16,
}

const READ_TIMEOUT: Duration = Duration::from_secs(1);

impl MgmtRssi {
    /// Fails with [`MgmtError::PermissionDenied`] without `CAP_NET_ADMIN`, so the caller
    /// can warn once and fall back to [`DiscoveryRssi`].
    pub fn new(index: u16) -> Result<Self, MgmtError> {
        if !has_net_admin() {
            return Err(MgmtError::PermissionDenied);
        }
        Ok(Self {
            socket: Arc::new(Mutex::new(MgmtSocket::open()?)),
            index,
        })
    }
}

impl RssiSource for MgmtRssi {
    fn scale(&self) -> RssiScale {
        RssiScale::MgmtRelative
    }

    async fn read(&self, address: Address) -> Option<f64> {
        let (socket, index) = (self.socket.clone(), self.index);
        // The socket is blocking and replies are matched per command, so serialise calls.
        let result = tokio::task::spawn_blocking(move || {
            let socket = socket.lock().unwrap_or_else(|e| e.into_inner());
            socket.connection_info(index, address, AddressType::BrEdr, READ_TIMEOUT)
        })
        .await
        .ok()?;
        match result {
            Ok(info) => info.rssi.map(f64::from),
            Err(MgmtError::NotConnected) => None,
            Err(e) => {
                tracing::debug!(%address, "mgmt rssi: {e}");
                None
            }
        }
    }
}
