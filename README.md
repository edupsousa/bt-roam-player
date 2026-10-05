# bt-roam-player

Plays one looping audio file and sends it to every Bluetooth speaker near you. Walk up to a
speaker and it is paired, connected and joins the sound; walk away and it is released, while the
loop keeps playing. Linux only (BlueZ and PipeWire).

## How it works

Proximity comes from Bluetooth signal strength (RSSI). A speaker is connected when its signal
stays strong for a couple of seconds and released when it stays weak. Up to `max_connected`
speakers play at once (about 3 is the limit of one adapter). New speakers in pairing mode can be
paired automatically.

The audio is decoded once and played through PipeWire, which resamples it for each speaker.

## Requirements

* Linux with **BlueZ** (`bluetoothd`) and **PipeWire** (with the Bluetooth module, so speakers
  show up as audio sinks).
* A Bluetooth adapter.
* Rust 1.88 or newer (edition 2024) to build.
* Development packages: PipeWire and libspa, D-Bus, BlueZ (libbluetooth), `pkg-config`, and
  `clang`/libclang (the PipeWire bindings are generated with bindgen).
* Optional: the `CAP_NET_ADMIN` capability, to read the signal of connected speakers (see
  [docs/USAGE.md](docs/USAGE.md#link-rssi-permission-cap_net_admin)). Without it, speakers are
  not released when you walk away.

## Build

```sh
cargo build --release
```

The binary is `target/release/bt-roam-player`.

## Run

```sh
bt-roam-player run --file loop.flac
```

Any format that Symphonia reads works (FLAC, MP3, WAV, Ogg, ...). Press Ctrl-C to stop; speakers
fade out and are disconnected, and a summary of the session (speakers seen, connections, playing
time) is printed.

| Command | What it does |
| --- | --- |
| `run -f FILE` | the player |
| `list [-s SECS] [-a]` | show speakers seen, their signal and whether they qualify |
| `pair [-s SECS]` | pair every speaker in pairing mode |
| `forget ADDRESS` | remove a paired speaker |

Global flags: `-c PATH` for a config file, `--adapter hciN` to pick the Bluetooth adapter
(default: BlueZ's default), `-v`/`-vv` for more logging (or `RUST_LOG`).
Configuration is optional; every key and its default is in
[config.example.toml](config.example.toml).

To keep it running in the background, see
[contrib/bt-roam-player.service](contrib/bt-roam-player.service) (a systemd user unit).

## Pairing is automatic by default

With `auto_pair = true` the player accepts pairing from any audio device in pairing mode nearby.
Set `auto_pair = false`, or use the `allow`/`deny` lists, if that is not what you want.

## More

* [docs/USAGE.md](docs/USAGE.md): behaviour in detail, what `run` prints, permissions, security
  and known issues.
* [docs/DESIGN.md](docs/DESIGN.md): design decisions.
* [docs/PLAN.md](docs/PLAN.md): milestones and results.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at
your option. Unless you state otherwise, any contribution you submit for inclusion is licensed
the same way, with no additional terms.
