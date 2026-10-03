# Design

`bt-roam-player` plays one looping audio file and routes it **simultaneously** to every
known Bluetooth speaker that is currently near the host, connecting and disconnecting
speakers as they come and go. The player never restarts.

Three subsystems:

1. **Bluetooth manager** (BlueZ over D-Bus): discovery, RSSI, pairing, connect/disconnect.
2. **Audio engine** (PipeWire): one looping playback stream, plus per-speaker linking and volume.
3. **Orchestrator**: per-speaker state machines that tie the two together.

> Revision notes: this version corrects several issues in the first draft. They are listed
> in [Appendix A](#appendix-a-changes-from-the-first-draft) so the reasoning isn't lost.

---

## Architecture

```
                         +----------------------------+
                         |  CLI (clap) + config.toml  |
                         +-------------+--------------+
                                       |
                                       v
+--------------------------------------------------------------------------+
|                    ORCHESTRATOR (tokio, single task)                     |
|        owns Map<Address, SpeakerActor>; routes events and commands       |
+--------+----------------------------------------------------+------------+
         | BtEvent / BtCommand (mpsc)          AudioEvent / AudioCommand (mpsc)
         v                                                    v
+---------------------------+                  +---------------------------------+
|   BLUETOOTH MANAGER       |                  |   AUDIO ENGINE (own OS thread)  |
|   (bluer, tokio)          |                  |   (pipewire-rs main loop)       |
|---------------------------|                  |---------------------------------|
| - Discovery session       |                  | - Looping playback stream       |
| - Device property stream  |                  |   (PCM decoded once, symphonia) |
| - RSSI source (see below) |                  | - Registry watcher (sink nodes) |
| - Pairing Agent           |                  | - Link create/destroy           |
| - Connect / Disconnect    |                  | - Per-sink volume               |
+-------------+-------------+                  +----------------+----------------+
              |                                                 |
              v                                                 v
           bluetoothd  ------------ A2DP ------------>  WirePlumber / PipeWire
                           (bluez5 monitor creates the
                            bluez_output.<MAC>.* sink node on connect)
```

The two backends never talk to each other. The orchestrator correlates a Bluetooth
device with its PipeWire sink node by **MAC address** (`api.bluez5.address` node
property).

---

## Key Design Decisions

### 1. Scope: dynamic discovery of any nearby speaker

The app has no pre-configured device list. It continuously looks for **any Bluetooth audio
sink** near the host, and connects (pairing first if needed) to the ones in range:

* **Candidate filter:** a device is a candidate if it advertises the A2DP Sink UUID
  (`0000110b-...`) or has an audio-rendering Class of Device (major class Audio/Video:
  loudspeaker, headphones, portable audio, ...). Everything else (phones, keyboards,
  watches) is ignored. UUIDs may not be resolved for never-seen devices, so Class of
  Device is the first-pass filter.
* **Already paired devices** (known to BlueZ) are connected directly.
* **Unpaired devices** are paired automatically through a registered agent that accepts
  SSP/PIN requests (see decision 3), then marked `Trusted`.
* **Optional config** (`config.toml`, every key has a default) holds only tuning and
  safety knobs: RSSI thresholds, `max_connected`, default volume, `auto_pair` on/off, and
  optional `deny`/`allow` filters by name pattern or address. No device list is needed.

Constraints that shape the behaviour:

* A speaker is only **discoverable/pairable while in pairing mode**. One that has never
  been paired with this host is picked up only when the user puts it in pairing mode;
  after that BlueZ remembers it and it is treated as paired.
* Speakers connected to another host usually stop advertising and are unavailable; the
  app doesn't try to steal them.
* A paired speaker that is merely powered on is often **connectable but not
  discoverable** (page scan only, no inquiry scan), so discovery may never report it and
  it has no pre-connection RSSI. For such devices, presence must be checked differently:
  periodically attempt a short-timeout connect as a probe, read RSSI from the live link,
  and drop the connection if it is too weak. This is validated in the M1 spike and may
  change the proximity logic for paired devices.
* **Security:** auto-pairing means any audio device in range that is in pairing mode gets
  paired. This is intentional for this tool, limited by the candidate filter and the
  optional `deny`/`allow` lists. `auto_pair = false` restricts the app to already paired
  devices. The agent auto-accepts only for devices that pass the candidate filter.

CLI: `bt-roam-player run --file track.flac` is the main mode; `list` shows what the app
currently sees; `forget <address>` removes a paired speaker.

### 2. Proximity: RSSI source, filtering and hysteresis

**RSSI availability is the biggest constraint.** BlueZ's `Device1.RSSI` is only updated
while discovery is running and the device is advertising or answering inquiries. For a
*connected* classic (BR/EDR) device, BlueZ generally stops reporting it. Disconnecting on
"RSSI < -80 dBm" therefore cannot rely on `Device1.RSSI` alone. Options:

| Source | Works when connected | Needs |
|--------|----------------------|-------|
| `Device1.RSSI` + discovery | Mostly no (classic) | nothing special |
| Kernel mgmt API `Get Connection Information` (returns RSSI of live ACL link) | Yes | `CAP_NET_ADMIN` |
| HCI `Read RSSI` | Yes | `CAP_NET_RAW` / root |

Decision: define an `RssiSource` trait with two implementations. `DiscoveryRssi` (BlueZ
property) is used to decide *when to connect*; `MgmtRssi` (mgmt socket) is used to decide
*when to disconnect*. If the capability is missing, fall back to `DiscoveryRssi` and log a
warning. A **spike is required early** (see PLAN.md, M1) to confirm real behaviour with the
target speakers, since it varies by controller and speaker.

Discovery also has a cost: inquiry on classic radios competes with active A2DP streams and
can cause audio glitches. Run discovery in **duty-cycled bursts** (e.g. 10 s on / 20 s off,
configurable) rather than continuously, and use the mgmt RSSI for already-connected
speakers.

**Filtering:**

* Samples arrive at irregular intervals, so use a **time-based EMA**:
  `alpha = 1 - exp(-dt / tau)` with `tau` around 3 s, instead of a fixed per-sample alpha.
* **Hysteresis on both edges**, with dwell times:
  * connect when smoothed RSSI > `-68 dBm` for >= 2 s
  * disconnect when smoothed RSSI < `-80 dBm` for >= 5 s
  * no RSSI sample at all for `stale_after` (e.g. 30 s) counts as "out of range"
* Thresholds are global defaults; optional per-device overrides (matched by address or
  name in the config) cover speakers whose transmit power differs a lot.

### 3. Pairing agent

* Register an `org.bluez.Agent1` (via `bluer::agent`) with capability `NoInputNoOutput`
  so SSP uses "just works". It is the default agent for the whole `run` session (unless
  `auto_pair = false`).
* SSP speakers: accept `RequestConfirmation`, `RequestAuthorization`, `AuthorizeService`.
* Legacy-PIN speakers: `RequestPinCode` must return a PIN **string** (default `"0000"`,
  configurable), it cannot simply return `Ok(())`.
* Requests from devices that fail the candidate filter or match `deny` are rejected.
* After a successful pair, set `Trusted = true` so BlueZ accepts reconnects.

### 4. Audio: one stream, fanned out to N sinks

The first draft's plan ("link the player to the speaker's sink, play to a virtual source")
mixed up terms and, as written, supported only one speaker.

* The player is a single **PipeWire playback stream** (a `Stream/Output/Audio` node), not a
  source. PCM is decoded once with `symphonia` into memory and the process callback loops
  over the buffer, so loops are gapless and there is no decoder on the realtime path.
  (Very large files: stream-decode into a ring buffer instead. Out of scope for v1.)
* Native PipeWire allows **one output port to be linked to many input ports**. To feed
  several speakers, the engine creates a link from the player's FL/FR ports to each
  speaker sink's playback FL/FR ports. Removing a speaker removes only its links.
* The stream is created with `node.autoconnect = false` (and `node.dont-reconnect = true`)
  so WirePlumber does **not** auto-route it to the default sink or move it around. Links
  are created with `object.linger = false` so they disappear if the app dies.
* If no speaker is connected the stream has no links. PipeWire pauses an unlinked stream,
  so loop position stalls. This is acceptable (the loop resumes on the next link) and
  documented behaviour; a "keep running against a null sink" mode is a possible later
  addition.
* The PipeWire main loop is not `Send`, so the audio engine lives on a dedicated OS thread
  and is controlled via `pipewire::channel` + events back over a tokio mpsc.

**Why not `rodio` or `libpulse`?** `rodio` (via cpal) hides the node and ports, so it can't
be linked to specific sinks. A PulseAudio-API stream can sit on only one sink at a time
(`move-sink-input`), so multi-speaker output would need extra loopback modules, adding
latency and complexity. Using `pipewire-rs` for everything also removes a second audio
dependency. The cost is more graph code, which is contained in `audio/graph.rs`.

**Known limitation, A2DP latency:** each speaker has its own codec/buffer latency (often
100-300 ms apart). Speakers in the *same room* will sound echoey. The intended use is
roaming between rooms, where this doesn't matter. A per-speaker delay compensation could be
added later, but isn't in scope.

**Known limitation, radio capacity:** controllers limit concurrent links and share
bandwidth; expect trouble beyond roughly 3 simultaneous A2DP speakers on one adapter.
Make `max_connected` configurable and have the orchestrator pick the strongest speakers
when over the limit.

### 5. Volume normalization

* When a speaker's sink node appears, set its volume to the configured level (default 60%)
  by writing the node's `Props` (`channelVolumes`/`volume`) through the PipeWire node
  proxy.
* WirePlumber may restore a previously saved volume shortly after the node appears. Apply
  the volume **after** the node reaches the `idle`/`running` state, and re-assert once if
  the value is changed within the first ~1 s. Verify during the spike.
* Optionally ramp from 0 to target over ~500 ms to avoid a click.
* Volume is applied to the sink node, so it also affects other apps using that speaker.
  Applying it to the link or a per-link volume stage is an alternative to evaluate.

---

## Per-Speaker State Machine

One actor per discovered candidate device (created on first sight, dropped after a long
absence), driven by Bluetooth events, audio events and timers.

```
                    +-------------+
        +---------> |   Absent    | <-------------------------------+
        |           +------+------+                                 |
        |                  | seen (RSSI sample)                     |
        |                  v                                        |
        |           +-------------+  RSSI > connect for dwell       |
        |           |   InRange   |---------------+                 |
        |           +-------------+               v                 |
        |        (unpaired: Pairing first; fail -> Backoff)           |
        |                              +----------------+  fail     |
        |                              |   Connecting   |--------+  |
        |                              +-------+--------+        |  |
        |                       Connected=true |                 v  |
        |                       (A2DP active)  v         +--------------+
        |                       +-----------------+      |   Backoff    |
        |                       |  AwaitingSink   |      | (exp. delay) |
        |                       +--------+--------+      +------+-------+
        |                 sink node seen |   timeout -> Backoff   | retry
        |                                v                        |
        |                       +-----------------+ <-------------+
        |                       | Linked & Playing|   (back to InRange)
        |                       +--------+--------+
        |   RSSI < disconnect for dwell  |  or Connected=false / sink node removed
        |                                v
        |                       +-----------------+
        +-----------------------+  Disconnecting  |
                                +-----------------+
```

Notes:

* **External changes are first-class**: the speaker powering off, the user disconnecting
  from the speaker, or another host taking it all arrive as `Connected=false` or sink-node
  removal, and move the actor to `Disconnecting`/`Absent` regardless of RSSI.
* **Failures** (connect error, `AwaitingSink` timeout, `org.bluez.Error.InProgress`, etc.)
  go to `Backoff` with exponential delay (e.g. 2 s up to 60 s), then re-evaluate.
* `Pairing` is entered from `InRange` for unpaired devices when `auto_pair` is on (agent
  accepts, `Trusted` is set, then `Connecting`). With `auto_pair = false`, unpaired
  devices stay `InRange` and are only reported.
* `max_connected` is enforced across actors: when over the limit, the weakest connected
  speaker is disconnected, and a candidate must beat it by a margin to replace it.
* Transitions to `Linked` require **both** `Device1.Connected` and the PipeWire sink node
  being present; the profile must be A2DP sink (UUID `0000110b-...`), not HFP.
* `Disconnecting` removes the link first (audio fades out), then calls `Disconnect`.

---

## Project Structure

```text
src/
├── main.rs              # clap CLI, subcommands: run, list, forget
├── config.rs            # optional config.toml: thresholds, limits, allow/deny filters
├── orchestrator.rs      # event loop, owns the speaker actors
├── speaker.rs           # per-speaker state machine (pure, unit-testable)
├── bluetooth/
│   ├── mod.rs
│   ├── adapter.rs       # duty-cycled discovery, device event stream
│   ├── agent.rs         # auto-accept pairing agent
│   ├── device.rs        # connect/disconnect/trust helpers
│   └── rssi.rs          # RssiSource trait, DiscoveryRssi, MgmtRssi
├── proximity.rs         # time-based EMA + hysteresis/dwell logic (pure)
└── audio/
    ├── mod.rs           # AudioEngine handle (commands in, events out)
    ├── decode.rs        # symphonia: file -> looped f32 PCM
    ├── stream.rs        # PipeWire playback stream + process callback
    ├── graph.rs         # registry watcher, link create/destroy, address -> node map
    └── volume.rs        # node Props volume control
```

`proximity.rs` and `speaker.rs` are deliberately free of I/O so the core logic can be
tested with synthetic RSSI traces and a fake clock.

---

## Recommended Crates

Versions checked against crates.io on 2026-10-03. Run `cargo add` to pick up current ones.

```toml
[dependencies]
clap       = { version = "4", features = ["derive"] }
tokio      = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
bluer      = { version = "0.17", features = ["bluetoothd"] }  # official BlueZ bindings
pipewire   = "0.10"            # graph, streams, links, node params (needs libpipewire >= 0.3)
symphonia  = { version = "0.6", features = ["all"] }  # decode flac/mp3/ogg/wav
serde      = { version = "1", features = ["derive"] }
toml       = "0.9"
tracing            = "0.1"
tracing-subscriber = "0.3"
anyhow     = "1"
thiserror  = "2"

[dev-dependencies]
tokio = { version = "1", features = ["test-util"] }  # paused clock for state-machine tests
```

`bluer` replaces raw `zbus`: it already models `Adapter`, `Device`, discovery sessions,
property-change streams and agents, so we avoid hand-writing D-Bus proxies. The mgmt-socket
RSSI reader will need a small amount of raw socket code (`libc`/`nix`).

---

## Runtime Requirements

* BlueZ 5.x with `bluetoothd` running; PipeWire with WirePlumber and its bluez5 monitor
  (SPA bluez plugin) enabled. Development machine: BlueZ 5.87, PipeWire 1.6.
* Runs as the logged-in user (needs the user's PipeWire socket and D-Bus session/system bus
  access). The mgmt RSSI source additionally needs `CAP_NET_ADMIN`
  (`setcap cap_net_admin+ep` on the binary).

---

## Appendix A: Changes from the first draft

| # | Problem in first draft | Resolution |
|---|------------------------|-----------|
| 1 | Disconnect rule depends on RSSI, but BlueZ doesn't report RSSI for connected classic devices | `RssiSource` abstraction; mgmt API for connected links; early spike |
| 2 | Continuous discovery degrades A2DP streaming | Duty-cycled discovery |
| 3 | Auto-pair anything in range: speakers are only pairable in pairing mode, and indiscriminate pairing is a risk | Kept, since dynamic discovery is a requirement. Limited by an audio-sink candidate filter, optional allow/deny lists and an `auto_pair` switch |
| 3b | Paired speakers that are on but not discoverable give no pre-connection RSSI | Probe-connect and read RSSI from the live link; to be validated in M1 |
| 4 | `RequestPinCode` "returning Ok(())" is invalid; it must return a PIN string | Documented |
| 5 | "Virtual source" is wrong; the player is an output stream. Linking one stream to one sink couldn't serve multiple speakers | One stream, fan-out links to N sinks |
| 6 | `libpulse` streams can only target one sink; `rodio` hides the node/ports | `pipewire-rs` for stream, links and volume |
| 7 | `bluez_sink.*` is the PulseAudio-era name; PipeWire uses `bluez_output.<MAC>.*` | Match on `api.bluez5.address` property |
| 8 | WirePlumber would auto-route/move the stream | `node.autoconnect=false`, `dont-reconnect` |
| 9 | Volume race with WirePlumber state restore | Apply after node is ready, re-assert once |
| 10 | State machine had no failure, backoff or external-disconnect handling, and fixed per-sample EMA ignored irregular sampling | Added states and time-based EMA |
| 11 | Stale crate versions (`zbus 4.4`, `rodio 0.19`) | Updated; `bluer` instead of `zbus` |
| 12 | Unmentioned constraints: A2DP latency skew, radio capacity, unlinked stream pauses | Documented as known limitations |
