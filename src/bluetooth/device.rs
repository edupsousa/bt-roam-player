//! Per-device helpers: a property snapshot, connect/disconnect with typed errors.

use bluer::{Address, Device, Uuid};

use super::candidate::DeviceInfo;

/// Failures of BlueZ operations, with the ones we handle specially pulled out of the
/// generic `org.bluez.Error.Failed` by their message.
#[derive(Debug, thiserror::Error)]
pub enum BtError {
    /// Paired in BlueZ but the speaker lost the key (or we did); re-pair needed.
    #[error("link key missing (br-connection-key-missing): the device must be paired again")]
    KeyMissing,
    /// Not reachable: powered off, out of range or not page-scanning. Takes ~5 s to fail.
    #[error("device did not answer the page (br-connection-page-timeout)")]
    PageTimeout,
    #[error("another operation is already in progress")]
    InProgress,
    #[error("already connected")]
    AlreadyConnected,
    #[error("adapter or device not ready")]
    NotReady,
    #[error("bluetooth: {0}")]
    Other(bluer::Error),
}

impl From<bluer::Error> for BtError {
    fn from(e: bluer::Error) -> Self {
        use bluer::ErrorKind as K;
        if e.message.contains("br-connection-key-missing") {
            return Self::KeyMissing;
        }
        if e.message.contains("br-connection-page-timeout") {
            return Self::PageTimeout;
        }
        match e.kind {
            K::InProgress => Self::InProgress,
            K::AlreadyConnected => Self::AlreadyConnected,
            K::NotReady => Self::NotReady,
            _ => Self::Other(e),
        }
    }
}

/// Everything we look at on a device, read in one go.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceSnapshot {
    pub address: Address,
    pub name: Option<String>,
    pub class: Option<u32>,
    pub uuids: Vec<Uuid>,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// Discovery RSSI in dBm. Frozen at its last pre-connection value once connected.
    pub rssi: Option<i16>,
}

impl DeviceSnapshot {
    pub async fn read(device: &Device) -> Result<Self, BtError> {
        Ok(Self {
            address: device.address(),
            name: device.name().await?,
            class: device.class().await?,
            uuids: device
                .uuids()
                .await?
                .unwrap_or_default()
                .into_iter()
                .collect(),
            paired: device.is_paired().await?,
            trusted: device.is_trusted().await?,
            connected: device.is_connected().await?,
            rssi: device.rssi().await?,
        })
    }

    pub fn info(&self) -> DeviceInfo<'_> {
        DeviceInfo {
            address: self.address,
            name: self.name.as_deref(),
            class: self.class,
            uuids: &self.uuids,
        }
    }
}

pub async fn connect(device: &Device) -> Result<(), BtError> {
    device.connect().await.map_err(Into::into)
}

pub async fn disconnect(device: &Device) -> Result<(), BtError> {
    device.disconnect().await.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bluer::ErrorKind;

    fn err(kind: ErrorKind, message: &str) -> bluer::Error {
        bluer::Error {
            kind,
            message: message.into(),
        }
    }

    #[test]
    fn maps_the_failures_seen_in_m1() {
        let e = err(ErrorKind::Failed, "br-connection-key-missing");
        assert!(matches!(BtError::from(e), BtError::KeyMissing));
        let e = err(ErrorKind::Failed, "br-connection-page-timeout");
        assert!(matches!(BtError::from(e), BtError::PageTimeout));
    }

    #[test]
    fn maps_error_kinds() {
        assert!(matches!(
            BtError::from(err(ErrorKind::InProgress, "")),
            BtError::InProgress
        ));
        assert!(matches!(
            BtError::from(err(ErrorKind::AlreadyConnected, "")),
            BtError::AlreadyConnected
        ));
        assert!(matches!(
            BtError::from(err(ErrorKind::Failed, "something else")),
            BtError::Other(_)
        ));
    }
}
