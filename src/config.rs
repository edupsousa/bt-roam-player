//! Optional `config.toml`: tuning and safety knobs only. Every key has a default and the
//! file itself may be absent. See DESIGN.md, decision 1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "bt-roam-player";
const FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Maximum simultaneously connected speakers; the strongest ones win.
    pub max_connected: usize,
    /// Volume applied to a speaker's sink when it appears (0.0..=1.0).
    pub default_volume: f32,
    /// Pair unknown candidates automatically. `false` restricts to already paired devices.
    pub auto_pair: bool,
    /// PIN returned to legacy-PIN speakers.
    pub pin: String,
    /// If non-empty, only devices matching one of these (address or name pattern) are used.
    pub allow: Vec<String>,
    /// Devices matching one of these (address or name pattern) are never used or paired.
    pub deny: Vec<String>,
    pub proximity: ProximityConfig,
    pub discovery: DiscoveryConfig,
    /// Per-device overrides, matched by address or name.
    #[serde(rename = "device")]
    pub devices: Vec<DeviceOverride>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProximityConfig {
    /// Connect when smoothed RSSI stays above this (dBm)...
    pub connect_rssi: i16,
    /// ...for at least this long.
    pub connect_dwell_secs: f64,
    /// Disconnect when smoothed RSSI stays below this (dBm)...
    pub disconnect_rssi: i16,
    /// ...for at least this long.
    pub disconnect_dwell_secs: f64,
    /// Time constant of the time-based EMA.
    pub ema_tau_secs: f64,
    /// No sample for this long counts as out of range.
    pub stale_after_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Duty-cycled discovery: scan for this long...
    pub on_secs: f64,
    /// ...then pause for this long (inquiry competes with active A2DP streams).
    pub off_secs: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceOverride {
    /// Bluetooth address (`AA:BB:CC:DD:EE:FF`) or name pattern this entry applies to.
    #[serde(rename = "match")]
    pub matcher: String,
    pub connect_rssi: Option<i16>,
    pub disconnect_rssi: Option<i16>,
    pub volume: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_connected: 3,
            default_volume: 0.6,
            auto_pair: true,
            pin: "0000".into(),
            allow: Vec::new(),
            deny: Vec::new(),
            proximity: ProximityConfig::default(),
            discovery: DiscoveryConfig::default(),
            devices: Vec::new(),
        }
    }
}

impl Default for ProximityConfig {
    fn default() -> Self {
        Self {
            connect_rssi: -68,
            connect_dwell_secs: 2.0,
            disconnect_rssi: -80,
            disconnect_dwell_secs: 5.0,
            ema_tau_secs: 3.0,
            stale_after_secs: 30.0,
        }
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            on_secs: 10.0,
            off_secs: 20.0,
        }
    }
}

impl Config {
    /// Load the config from `explicit` (must exist), else from the first default location
    /// that exists (`./config.toml`, then `$XDG_CONFIG_HOME/bt-roam-player/config.toml`),
    /// else fall back to defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        if let Some(path) = explicit {
            return Self::from_file(path);
        }
        match default_locations().into_iter().find(|p| p.is_file()) {
            Some(path) => Self::from_file(&path),
            None => Ok(Self::default()),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.max_connected >= 1, "max_connected must be at least 1");
        check_volume("default_volume", self.default_volume)?;
        self.proximity.validate()?;
        ensure!(
            self.discovery.on_secs > 0.0 && self.discovery.off_secs >= 0.0,
            "discovery.on_secs must be > 0 and discovery.off_secs >= 0"
        );
        for dev in &self.devices {
            ensure!(
                !dev.matcher.is_empty(),
                "[[device]] entry with empty `match`"
            );
            if let Some(v) = dev.volume {
                check_volume("device.volume", v)?;
            }
        }
        Ok(())
    }
}

impl ProximityConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.disconnect_rssi < self.connect_rssi,
            "proximity.disconnect_rssi ({}) must be below connect_rssi ({}) for hysteresis",
            self.disconnect_rssi,
            self.connect_rssi
        );
        for (name, v) in [
            ("connect_dwell_secs", self.connect_dwell_secs),
            ("disconnect_dwell_secs", self.disconnect_dwell_secs),
        ] {
            ensure!(v >= 0.0, "proximity.{name} must be >= 0");
        }
        for (name, v) in [
            ("ema_tau_secs", self.ema_tau_secs),
            ("stale_after_secs", self.stale_after_secs),
        ] {
            ensure!(v > 0.0, "proximity.{name} must be > 0");
        }
        Ok(())
    }
}

fn check_volume(name: &str, v: f32) -> Result<()> {
    if !(0.0..=1.0).contains(&v) {
        bail!("{name} must be between 0.0 and 1.0, got {v}");
    }
    Ok(())
}

fn default_locations() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(FILE_NAME)];
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(dir) = xdg {
        paths.push(dir.join(APP_DIR).join(FILE_NAME));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let cfg = Config::parse(
            r#"
            max_connected = 2
            deny = ["AA:BB:CC:DD:EE:FF"]

            [proximity]
            connect_rssi = -60
            "#,
        )
        .unwrap();
        assert_eq!(cfg.max_connected, 2);
        assert_eq!(cfg.deny, ["AA:BB:CC:DD:EE:FF"]);
        assert_eq!(cfg.proximity.connect_rssi, -60);
        assert_eq!(cfg.proximity.disconnect_rssi, -80);
        assert!(cfg.auto_pair);
    }

    #[test]
    fn per_device_overrides_parse() {
        let cfg = Config::parse(
            r#"
            [[device]]
            match = "JBL*"
            connect_rssi = -75
            volume = 0.4

            [[device]]
            match = "11:22:33:44:55:66"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.devices.len(), 2);
        assert_eq!(cfg.devices[0].matcher, "JBL*");
        assert_eq!(cfg.devices[0].connect_rssi, Some(-75));
        assert_eq!(cfg.devices[1].volume, None);
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config {
            allow: vec!["Kitchen*".into()],
            devices: vec![DeviceOverride {
                matcher: "Kitchen*".into(),
                volume: Some(0.3),
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), cfg);
    }

    #[test]
    fn default_round_trips() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), Config::default());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("max_conected = 2").is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(Config::parse("max_connected = 0").is_err());
        assert!(Config::parse("default_volume = 1.5").is_err());
        let inverted = "[proximity]\nconnect_rssi = -80\ndisconnect_rssi = -70";
        assert!(Config::parse(inverted).is_err());
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        assert!(Config::load(Some(Path::new("/nonexistent/config.toml"))).is_err());
    }
}
