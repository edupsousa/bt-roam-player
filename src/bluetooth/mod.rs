//! Bluetooth side: BlueZ adapter/devices through `bluer`, plus RSSI sources.

// Pieces are wired up by later milestones (M5/M6).
#![allow(dead_code)]

pub mod adapter;
pub mod candidate;
pub mod device;
pub mod mgmt;
pub mod rssi;
