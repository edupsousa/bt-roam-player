//! Auto-pairing: an `Agent1` that accepts requests only from audio candidates, and the
//! pair flow (pair, trust, verify the bond). See DESIGN.md, decision 3.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bluer::agent::{Agent, AgentHandle, ReqError, ReqResult};
use bluer::{Adapter, Address};
use dbus::nonblock::{Proxy, SyncConnection, stdintf::org_freedesktop_dbus::Properties};
use tokio::task::JoinHandle;

use super::candidate::{Verdict, evaluate};
use super::device::{BtError, DeviceSnapshot};
use crate::config::Config;

const PAIR_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("auto_pair is disabled")]
    Disabled,
    #[error("not an audio candidate: {0:?}")]
    NotCandidate(Verdict),
    #[error("pairing timed out")]
    Timeout,
    /// `Paired` but never `Bonded`: the link key was not stored (adapter not pairable?).
    #[error("paired but not bonded: the link key was not stored")]
    NotBonded,
    #[error(transparent)]
    Bt(#[from] BtError),
    #[error("reading bonded state: {0}")]
    Dbus(String),
}

impl From<bluer::Error> for PairError {
    fn from(e: bluer::Error) -> Self {
        Self::Bt(e.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    AlreadyBonded,
    Paired,
}

/// Reads `Device1.Bonded`, which bluer does not expose, straight from D-Bus.
struct BondedReader {
    conn: Arc<SyncConnection>,
    task: JoinHandle<()>,
}

impl BondedReader {
    fn connect() -> Result<Self> {
        let (resource, conn) =
            dbus_tokio::connection::new_system_sync().context("connecting to the system bus")?;
        let task = tokio::spawn(async move {
            let err = resource.await;
            tracing::warn!("lost D-Bus connection: {err}");
        });
        Ok(Self { conn, task })
    }

    async fn bonded(&self, adapter: &str, address: Address) -> Result<bool, PairError> {
        let path = format!(
            "/org/bluez/{adapter}/dev_{}",
            address.to_string().replace(':', "_")
        );
        Proxy::new("org.bluez", path, Duration::from_secs(5), self.conn.clone())
            .get::<bool>("org.bluez.Device1", "Bonded")
            .await
            .map_err(|e| PairError::Dbus(e.to_string()))
    }
}

impl Drop for BondedReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Pairing {
    adapter: Adapter,
    bonded: BondedReader,
    config: Config,
    // Unregisters the agent on drop.
    _agent: Option<AgentHandle>,
}

impl Pairing {
    /// Register the agent (unless `auto_pair` is off) and prepare the pair flow.
    pub async fn start(
        session: &bluer::Session,
        adapter: Adapter,
        config: &Config,
    ) -> Result<Self> {
        let agent = if config.auto_pair {
            Some(
                session
                    .register_agent(make_agent(adapter.clone(), config))
                    .await
                    .context("registering the pairing agent")?,
            )
        } else {
            None
        };
        Ok(Self {
            adapter,
            bonded: BondedReader::connect()?,
            config: config.clone(),
            _agent: agent,
        })
    }

    pub async fn is_bonded(&self, address: Address) -> Result<bool, PairError> {
        self.bonded.bonded(self.adapter.name(), address).await
    }

    /// Make sure `address` is paired, trusted and bonded. A device that is `Paired` but not
    /// `Bonded` (stale or never-stored key) is removed from BlueZ and `NotBonded` is returned;
    /// the caller pairs again once discovery sees the device (in pairing mode) again.
    pub async fn ensure_paired(&self, address: Address) -> Result<PairOutcome, PairError> {
        if !self.config.auto_pair {
            return Err(PairError::Disabled);
        }
        let device = self.adapter.device(address)?;
        let snapshot = DeviceSnapshot::read(&device).await?;
        let verdict = evaluate(&snapshot.info(), &self.config.allow, &self.config.deny);
        if !verdict.is_accepted() {
            return Err(PairError::NotCandidate(verdict));
        }
        if snapshot.paired && self.is_bonded(address).await? {
            if !snapshot.trusted {
                device.set_trusted(true).await?;
            }
            return Ok(PairOutcome::AlreadyBonded);
        }
        if snapshot.paired {
            tracing::warn!(%address, "paired but not bonded, removing and pairing again");
            self.adapter.remove_device(address).await?;
            // The device is gone from BlueZ until discovery sees it again.
            return Err(PairError::NotBonded);
        }
        // The system has been seen to turn this off by itself (DESIGN.md, decision 3).
        if !self.adapter.is_pairable().await? {
            tracing::warn!("adapter had Pairable off, turning it on");
            self.adapter.set_pairable(true).await?;
        }
        tokio::time::timeout(PAIR_TIMEOUT, device.pair())
            .await
            .map_err(|_| PairError::Timeout)??;
        device.set_trusted(true).await?;
        if !self.is_bonded(address).await? {
            let _ = self.adapter.remove_device(address).await;
            return Err(PairError::NotBonded);
        }
        Ok(PairOutcome::Paired)
    }

    /// Remove a device (and its bond) from BlueZ.
    pub async fn forget(&self, address: Address) -> Result<(), BtError> {
        self.adapter
            .remove_device(address)
            .await
            .map_err(Into::into)
    }
}

struct AgentContext {
    adapter: Adapter,
    allow: Vec<String>,
    deny: Vec<String>,
    pin: String,
}

impl AgentContext {
    /// Whether to serve a request from `address`: only audio candidates that are not denied.
    async fn vet(&self, address: Address, what: &str) -> ReqResult<()> {
        let snapshot = match self.adapter.device(address) {
            Ok(device) => DeviceSnapshot::read(&device).await.ok(),
            Err(_) => None,
        };
        let Some(snapshot) = snapshot else {
            tracing::warn!(%address, "{what}: unknown device, rejecting");
            return Err(ReqError::Rejected);
        };
        match evaluate(&snapshot.info(), &self.allow, &self.deny) {
            Verdict::Accept => {
                tracing::info!(%address, name = ?snapshot.name, "{what}: accepting");
                Ok(())
            }
            verdict => {
                tracing::warn!(%address, name = ?snapshot.name, "{what}: rejecting ({verdict:?})");
                Err(ReqError::Rejected)
            }
        }
    }
}

fn make_agent(adapter: Adapter, config: &Config) -> Agent {
    let ctx = Arc::new(AgentContext {
        adapter,
        allow: config.allow.clone(),
        deny: config.deny.clone(),
        pin: config.pin.clone(),
    });
    let (c1, c2, c3, c4) = (ctx.clone(), ctx.clone(), ctx.clone(), ctx);
    Agent {
        request_default: true,
        request_pin_code: Some(Box::new(move |req| {
            let ctx = c1.clone();
            Box::pin(async move {
                ctx.vet(req.device, "pin request").await?;
                Ok(ctx.pin.clone())
            })
        })),
        request_confirmation: Some(Box::new(move |req| {
            let ctx = c2.clone();
            Box::pin(async move { ctx.vet(req.device, "confirmation").await })
        })),
        request_authorization: Some(Box::new(move |req| {
            let ctx = c3.clone();
            Box::pin(async move { ctx.vet(req.device, "authorization").await })
        })),
        authorize_service: Some(Box::new(move |req| {
            let ctx = c4.clone();
            Box::pin(async move { ctx.vet(req.device, "service authorization").await })
        })),
        ..Default::default()
    }
}
