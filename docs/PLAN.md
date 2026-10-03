# Implementation Plan

High-level, milestone-ordered. Each milestone ends in something runnable. See
[DESIGN.md](DESIGN.md) for the rationale behind each piece.

The riskiest unknowns (RSSI on connected links, WirePlumber volume/route behaviour,
multi-sink fan-out) are tackled first as small spikes, before the architecture is built on
top of assumptions.

## Working agreement: update this plan after every milestone

This plan is a living document. Before a milestone is considered finished, and before
starting the next one, do all of the following in the same commit as the milestone's code:

1. **Mark it done.** Change the milestone heading to `## M<n>: <title> [DONE <YYYY-MM-DD>]`
   (use `[IN PROGRESS]` while working, nothing while not started).
2. **Document deviations.** Add a `**Outcome / deviations:**` paragraph under the milestone:
   what was actually built, anything dropped, added or done differently from the bullets
   above, and why. Link the relevant commit or file where useful. If the milestone went
   exactly to plan, say so in one line.
3. **Update downstream tasks.** Re-read every later milestone and edit it to reflect what was
   learned: adjust scope, add newly discovered tasks, remove obsolete ones, and fix any
   assumption that turned out to be wrong. New work that doesn't fit an existing milestone
   goes into a new milestone or into "Later / out of scope".
4. **Sync DESIGN.md.** If a finding changes a design decision (especially after the M1
   spikes), update DESIGN.md and note the change in its appendix.

## M0: Project scaffold

* `cargo init` in `bluetooth-audio-player`, add dependencies from DESIGN.md.
* `tracing` setup, `clap` skeleton with subcommands `run`, `pair`, `list`.
* `config.rs`: load `config.toml` (thresholds, `max_connected`, speaker allowlist with
  address/alias/volume/threshold overrides). Unit tests for parsing and defaults.
* CI-ish checks: `cargo fmt`, `cargo clippy`, `cargo nextest run`.

**Done when:** `bt-roam-player list --help` works and config round-trips in tests.

## M1: Spikes (throwaway code, findings written back to DESIGN.md)

1. **RSSI on connected devices.** With a real speaker: log `Device1.RSSI` before, during and
   after connection; then read RSSI via the mgmt socket (`Get Connection Information`).
   Decide whether `MgmtRssi` is viable and what capability it needs.
2. **Discovery vs. streaming.** Play audio over A2DP while running discovery; check for
   glitches to choose duty-cycle values.
3. **PipeWire fan-out.** Minimal `pipewire-rs` program: create a playback stream with
   `autoconnect=false`, link it by hand to two sinks (can be a real speaker plus the local
   output), confirm both play.
4. **Volume.** Set node volume on a freshly appearing BT sink; check whether WirePlumber
   overrides it.

**Done when:** each question has a written answer and DESIGN.md is adjusted if reality
differed.

## M2: Audio engine (no Bluetooth)

* `audio/decode.rs`: symphonia decode to f32 PCM at the graph's sample rate; resample if
  needed.
* `audio/stream.rs`: looping playback stream on a dedicated PipeWire thread.
* `audio/graph.rs`: registry listener that tracks sink nodes and exposes
  `address -> node id` (from `api.bluez5.address`); `link(node)` / `unlink(node)`.
* `audio/volume.rs`: set/ramp node volume.
* `AudioEngine` handle: `AudioCommand::{Link, Unlink, SetVolume}` in,
  `AudioEvent::{SinkAppeared, SinkRemoved}` out.
* Dev CLI: `bt-roam-player` plays a file to chosen node names, for testing without BT logic.

**Done when:** a file loops gaplessly and can be linked/unlinked to any sink live, with
volume set.

## M3: Proximity logic (pure code)

* `proximity.rs`: time-based EMA, hysteresis with connect/disconnect dwell, stale-sample
  timeout. Driven by explicit timestamps, no I/O.
* Property-style tests with synthetic noisy RSSI traces (boundary jitter must not flap).

**Done when:** tests prove no flapping on a jittery boundary trace and correct timing of
both transitions.

## M4: Bluetooth manager

* `bluetooth/adapter.rs`: power on adapter, duty-cycled discovery, stream of device
  property changes normalised to `BtEvent`.
* `bluetooth/rssi.rs`: `RssiSource` trait, `DiscoveryRssi`, and `MgmtRssi` (per M1 result).
* `bluetooth/device.rs`: connect, disconnect, trusted/paired checks, A2DP UUID check,
  `Connected` change events. Map BlueZ errors (`InProgress`, `AlreadyConnected`,
  `Failed`, ...) to a typed error.
* Dev CLI `list`: prints nearby/known audio devices with smoothed RSSI.

**Done when:** `list` shows live RSSI for allowlisted speakers, including once connected
(if M1 found it feasible).

## M5: Pair mode

* `bluetooth/agent.rs`: `Agent1` implementation (SSP accept, configurable legacy PIN).
* `pair` subcommand: scan, show audio sinks, interactive selection, pair + trust, optionally
  append to `config.toml`.

**Done when:** a speaker in pairing mode can be paired and trusted from the CLI and
reconnects later without the agent.

## M6: Speaker state machine and orchestrator

* `speaker.rs`: states and transitions from DESIGN.md as a pure function
  `(State, Event, Instant) -> (State, Vec<Action>)`. Table-driven tests for every
  transition, including failure/backoff and external disconnects.
* `orchestrator.rs`: owns one actor per allowlisted speaker, fans events in from BT and
  audio, executes actions (connect, link, set volume, unlink, disconnect), enforces
  `max_connected` (keep the strongest).
* Wire `run` subcommand end to end; graceful shutdown on SIGINT/SIGTERM (unlink, optional
  disconnect, stop stream).

**Done when:** walking toward/away from a real speaker connects, links, sets volume, then
unlinks and disconnects, while the loop keeps playing.

## M7: Hardening

* Handle `bluetoothd` or PipeWire restarts (reconnect to bus/daemon, rebuild state).
* Multi-speaker soak test (2-3 speakers, hours), check for leaks and stuck states.
* Logging review (state transitions at `info`, raw RSSI at `trace`), clear startup errors
  for missing permissions, unpaired speakers, absent adapter.
* Docs: README with setup (`setcap`, pairing, config example), systemd user unit.

## Later / out of scope for v1

* Opt-in `--auto-pair` in run mode.
* Per-speaker delay compensation for same-room use.
* Keeping the loop position running with no speakers (null-sink mode).
* Streaming decode for very large files; playlists.
* LE Audio / Auracast.
* Status/control interface (Unix socket or D-Bus).

## Testing strategy

* **Unit:** `proximity`, `speaker`, `config` (no hardware, fake clock via tokio paused time
  or explicit `Instant` parameters).
* **Integration (manual, hardware):** M1 spikes, M4 `list`, M5 pairing, M6 roaming test.
* Hardware-dependent tests are `#[ignore]`d and run with `cargo nextest run --run-ignored`.
