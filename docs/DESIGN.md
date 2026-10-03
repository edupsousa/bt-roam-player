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
  (`0000110b-...`) or its Class of Device is major class Audio/Video **and** its minor
  class is an audio *output* (loudspeaker `0x05`, headphones `0x06`, portable audio
  `0x07`, car audio `0x08`, hi-fi `0x0a`; the minor class is `(class >> 2) & 0x3f`).
  Everything else (phones, keyboards, watches) is ignored. UUIDs may not be resolved for
  never-seen devices, so Class of Device is the first-pass filter.
  **Spike finding:** matching the major class alone is too loose. Neighbouring TVs
  (class `0x0c043c` / `0x08043c`, minor `0x0f` "video display and loudspeaker") passed it,
  and one advertised A2DP sink too. Video-display minor classes (`0x0c`-`0x0f`) are
  rejected even if A2DP is advertised, unless the device is on the `allow` list.
  Real samples: JBL GO 2 `0x200414`, Echo Studio `0x2c0414`, WF-1000XM5 `0x240404`.
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
  and drop the connection if it is too weak.
  **Spike finding (JBL GO 2):** a paired speaker that is powered on but not in pairing
  mode is reported by discovery with **no RSSI**, and **reconnects by itself** within a
  few seconds of power-on (the speaker initiates, given a stored bond). A probe-connect to
  a powered-off speaker blocks for **~5.2 s** and fails with `br-connection-page-timeout`
  (measured twice). So: (a) the app cannot gate *connecting* on RSSI for such speakers;
  presence is "Connected became true", and proximity only governs *disconnecting*
  (mgmt RSSI) plus a cool-down so a speaker we dropped for being too far is not
  immediately re-accepted; (b) probes are a fallback only, rate-limited (a 5 s page scan
  occupies the radio; its effect on other active streams is **not yet measured**, to be
  tested with two speakers in M6/M7).
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

**Spike findings (M1, JBL GO 2, Intel-class laptop controller):**

* `Device1.RSSI` is frozen at its last pre-connection value once connected, even while
  discovery runs. It is unusable for disconnect decisions, as predicted.
* The mgmt `Get Connection Information` call works on the live link but needs
  `CAP_NET_ADMIN` (`Permission Denied` otherwise). There is no group that grants it; it
  must be put on the process (on NixOS: `security.wrappers`; elsewhere `setcap` on a
  binary outside a read-only store).
* **mgmt RSSI is relative, not dBm.** For BR/EDR it is measured against the controller's
  "golden receive power range": `0` means inside the ideal window, negative means below
  it. Walking away and back gave `0 -> -5 -> -11 -> -20 -> -24 -> -27 -> ... -8 -> 0`, and
  the link stayed up throughout. The values update in steps about every **3 s**.
  Therefore the `-68/-80 dBm` thresholds apply only to discovery RSSI (true dBm); mgmt
  thresholds are separate and expressed in dB below the golden range (provisional defaults:
  re-accept above `-10`, disconnect below `-25`), and the dwell/EMA times must account for
  the ~3 s update period (`tau` >= 5 s, disconnect dwell >= 6 s).
  **Implemented (M3):** `src/proximity.rs` with `Params::dbm`/`Params::mgmt`; mgmt defaults are
  `[proximity.mgmt]` `connect_db -10`, `disconnect_db -25`, dwells 3 s / 6 s, tau 5 s, plus a
  shared `cooldown_secs = 30` that starts whenever a speaker is dropped as too far.

Discovery also has a cost: inquiry on classic radios competes with active A2DP streams and
can cause audio glitches. Run discovery in **duty-cycled bursts** (e.g. 10 s on / 20 s off,
configurable) rather than continuously, and use the mgmt RSSI for already-connected
speakers. *Spike result:* one speaker over SBC with 20 s of continuous discovery gave no
audible glitches (listened to twice), so the duty cycle is a precaution rather than a
proven need; re-check with 2-3 speakers in M7.

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
* **The adapter must be `Pairable = true` while pairing** (the app sets it on startup).
  Spike finding: with `Pairable: no` the host sends "No Bonding" in its IO capability
  reply, the kernel reports the link key with `Store hint: No`, and BlueZ never stores it.
  Pairing then *appears* to succeed (`Paired: yes`, `Bonded: no`) but the bond vanishes
  within minutes, and the next connect fails with `br-connection-key-missing`. This
  adapter had `Pairable: no` (it happens intermittently on this system and also affected
  other devices, so something else seems to toggle it). Pair success must be judged by
  `Bonded`, not `Paired`; on `br-connection-key-missing`, `RemoveDevice` and re-pair.

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
  by writing the **device's output `Route`** (`channelVolumes`, `save=false`) through the
  PipeWire device proxy, i.e. what `wpctl set-volume` does. Writing the *node's* `Props`
  is a separate, multiplicative stage on Bluetooth sinks: the restored route volume still
  applies on top, so a node-only write is far too quiet and `wpctl` does not reflect it
  (M2 finding). Non-Bluetooth sinks have no route and get the node `Props` write. The
  route's `index`/`device` are learned from the device's `Route` params, and the node is
  kept at 1.0. Values are perceptual (cubic, like `wpctl`).
* WirePlumber may restore a previously saved volume shortly after the node appears. Apply
  the volume **after** the node reaches the `idle`/`running` state, and re-assert once if
  the value is changed within the first ~1 s. Verify during the spike.
  **Spike result:** WirePlumber restores a previously saved volume when the sink appears
  (observed 0.66, 0.39, 0.13 on successive connections), and an explicit write immediately
  after the sink appeared stuck for the 5 s observed, both during playback and just after
  reconnect. No override was seen, so the "re-assert once after ~1 s" step stays as a cheap
  safeguard, not a proven need. (M2 confirmed the route write holds and is visible in `wpctl`.)
* Ramp from 0 to target (default ~500 ms). The link is created only after the first volume
  write, because otherwise the stream opens at the speaker's restored volume and clicks
  (heard on the JBL; with the ordering fixed, no click).
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
    └── volume.rs        # device Route (BT) / node Props volume, ramps
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
| 13 | M1: audio major class alone matches TVs | Candidate filter also checks the minor class and rejects video-display minors |
| 14 | M1: mgmt RSSI is relative to the golden range, not dBm, and updates ~every 3 s | Separate mgmt thresholds, longer EMA/dwell |
| 15 | M1: `Pairable: no` makes pairing succeed without bonding | App sets `Pairable = true`; judge success by `Bonded`; re-pair on `br-connection-key-missing` |
| 16 | M1: paired speakers reconnect on their own and have no pre-connection RSSI | Proximity governs disconnect plus a re-accept cool-down; probes are a rate-limited fallback (~5 s page timeout) |
| 17 | M1: fan-out, volume and discovery-during-streaming confirmed with one speaker | No design change; multi-speaker checks moved to M6/M7 |
| 18 | M2: node `Props` volume is multiplicative with the BT device route volume | Volume is written to the device `Route` (as `wpctl` does); link only after the first volume write |
| 19 | M2: registry node globals omit `api.bluez5.address` | Address parsed from `bluez_output.<MAC>.N` node name |
| 20 | M2: resampling not needed | Clip kept at native rate, PipeWire resamples per sink |
