//! M1 spike (throwaway): BlueZ probing.
//!
//!   spike_bt dump                 known devices: class, UUIDs, paired/connected, RSSI
//!   spike_bt watch [secs]         run discovery, log every device add/property change
//!   spike_bt connect <addr> [s]   connect, then log RSSI/props for s seconds
use std::time::{Duration, Instant};

use bluer::{AdapterEvent, Address, DeviceEvent};
use futures::{StreamExt, pin_mut};

const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";

fn major_class(class: u32) -> u32 {
    (class >> 8) & 0x1f
}

async fn describe(adapter: &bluer::Adapter, addr: Address) -> bluer::Result<()> {
    let dev = adapter.device(addr)?;
    let class = dev.class().await?;
    let uuids = dev.uuids().await?.unwrap_or_default();
    let a2dp = uuids.iter().any(|u| u.to_string() == A2DP_SINK);
    let audio_class = class.map(|c| major_class(c) == 0x04).unwrap_or(false);
    println!(
        "{addr} {:<24} class={} a2dp_sink={a2dp} audio_class={audio_class} paired={} conn={} trusted={} rssi={:?} icon={:?} -> candidate={}",
        dev.name().await?.unwrap_or_default(),
        class.map(|c| format!("{c:#08x}")).unwrap_or("-".into()),
        dev.is_paired().await?,
        dev.is_connected().await?,
        dev.is_trusted().await?,
        dev.rssi().await?,
        dev.icon().await?,
        a2dp || audio_class,
    );
    Ok(())
}

#[tokio::main]
async fn main() -> bluer::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    let t0 = Instant::now();

    match args.get(1).map(String::as_str) {
        Some("dump") => {
            for addr in adapter.device_addresses().await? {
                describe(&adapter, addr).await?;
            }
        }
        Some("watch") => {
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
            let events = adapter.discover_devices_with_changes().await?;
            pin_mut!(events);
            let deadline = tokio::time::sleep(Duration::from_secs(secs));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    Some(ev) = events.next() => match ev {
                        AdapterEvent::DeviceAdded(a) => {
                            print!("[{:6.1}s] ADDED   ", t0.elapsed().as_secs_f32());
                            describe(&adapter, a).await?;
                        }
                        AdapterEvent::DeviceRemoved(a) => {
                            println!("[{:6.1}s] REMOVED {a}", t0.elapsed().as_secs_f32());
                        }
                        AdapterEvent::PropertyChanged(p) => {
                            println!("[{:6.1}s] adapter {p:?}", t0.elapsed().as_secs_f32());
                        }
                    }
                }
            }
        }
        Some("connect") => {
            let addr: Address = args[2].parse().expect("address");
            let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            let dev = adapter.device(addr)?;
            let events = dev.events().await?;
            pin_mut!(events);
            println!(
                "[{:6.1}s] RSSI before connect: {:?}",
                t0.elapsed().as_secs_f32(),
                dev.rssi().await?
            );
            let started = Instant::now();
            let res = dev.connect().await;
            println!(
                "[{:6.1}s] connect() -> {res:?} after {:?}",
                t0.elapsed().as_secs_f32(),
                started.elapsed()
            );
            let deadline = tokio::time::sleep(Duration::from_secs(secs));
            tokio::pin!(deadline);
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    _ = tick.tick() => println!(
                        "[{:6.1}s] poll rssi={:?} tx={:?} connected={}",
                        t0.elapsed().as_secs_f32(), dev.rssi().await?, dev.tx_power().await?, dev.is_connected().await?
                    ),
                    Some(DeviceEvent::PropertyChanged(p)) = events.next() => {
                        println!("[{:6.1}s] prop {p:?}", t0.elapsed().as_secs_f32());
                    }
                }
            }
        }
        _ => eprintln!("usage: spike_bt dump | watch [secs] | connect <addr> [secs]"),
    }
    Ok(())
}
