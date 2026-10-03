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

## M0: Project scaffold [DONE 2026-10-03]

* `cargo init` in `bluetooth-audio-player`, add dependencies from DESIGN.md.
* `tracing` setup, `clap` skeleton with subcommands `run`, `list`, `forget`.
* `config.rs`: load an optional `config.toml` (thresholds, `max_connected`, default volume,
  `auto_pair`, `allow`/`deny` filters, optional per-device overrides). Every key has a
  default and the file may be absent. Unit tests for parsing and defaults.
* CI-ish checks: `cargo fmt`, `cargo clippy`, `cargo nextest run`.

**Done when:** `bt-roam-player list --help` works and config round-trips in tests.

**Outcome / deviations:** Built as planned. Notes:
* `toml` resolved to 1.x (DESIGN listed 0.9); `tracing-subscriber` needs the `env-filter`
  feature (`-v/-vv` or `RUST_LOG`). `pipewire`, `symphonia` and `thiserror` are added but
  unused until M2/M4, so `cargo clippy` warns about unused dependencies until then.
* `config.rs` also exposes the discovery duty cycle (`[discovery]`), `pin`, and validation
  (e.g. `disconnect_rssi < connect_rssi`); unknown keys are rejected. Config lookup order:
  `--config`, `./config.toml`, `$XDG_CONFIG_HOME/bt-roam-player/config.toml`, defaults.
* `allow`/`deny`/`[[device]].match` are plain strings for now; matching (address vs name
  glob) is implemented in M4's candidate filter.
* Subcommands `run`, `list`, `forget` parse arguments but bail with "not implemented yet".
* Added `config.example.toml`.

## M1: Spikes (throwaway code, findings written back to DESIGN.md) [DONE 2026-10-03]

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
5. **Paired-but-not-discoverable speakers.** Power on a previously paired speaker and
   check whether discovery ever reports it (and with what RSSI). If not, prototype the
   probe-connect approach (short-timeout connect, then read link RSSI) and measure how long
   a failed probe takes and whether it disturbs other active streams.
6. **Candidate filter.** Dump Class of Device and UUIDs for the speakers/headphones at
   hand (unpaired and paired) and confirm the filter selects them and rejects phones etc.

**Done when:** each question has a written answer and DESIGN.md is adjusted if reality
differed.

**Outcome / deviations:** All six questions answered with one real speaker (JBL GO 2); the
details are in DESIGN.md (decisions 1-5 and Appendix A rows 13-17). Throwaway code is in
`examples/spike_bt.rs` (BlueZ dump/watch/connect) and `examples/spike_pw.rs` (PipeWire
fan-out); `futures` and `libspa-sys` were added as dev-dependencies for them.
1. **RSSI:** `Device1.RSSI` freezes once connected. mgmt RSSI works but needs
   `CAP_NET_ADMIN`, is **relative to the golden range** (0 = ideal, down to -27 at the far
   point of a walk) and updates about every 3 s. Only checked with `btmgmt` (not our own
   mgmt-socket code, which is M4).
2. **Discovery vs streaming:** no audible glitches with 20 s of continuous discovery (one
   speaker, SBC; listening test, no measurement).
3. **Fan-out:** one `autoconnect=false` stream linked to the laptop output and the JBL via
   `link-factory` (`object.linger=false`); both audible.
4. **Volume:** WirePlumber restores a saved volume on connect; an explicit write stuck. Tested
   with `wpctl`, so setting `Props` through the node proxy is still to be verified in M2.
5. **Paired-but-not-discoverable:** powered-on paired speaker shows no RSSI in discovery and
   reconnects itself; a failed probe-connect takes ~5.2 s (`br-connection-page-timeout`).
   The effect of a probe on other streams was **not measured** (needs two speakers).
6. **Candidate filter:** major class alone also matches TVs; minor-class check added.
* **Unplanned finding:** pairing silently failed to bond because the adapter had
  `Pairable: no` (see DESIGN decision 3). The three earlier pairing failures in this
  milestone were caused by it. The adapter setting is not persistent on this machine, so M4
  sets it explicitly.

**Downstream changes made after M1:** see the edits to M2-M7 below.

## M2: Audio engine (no Bluetooth) [DONE 2026-10-03]

* `audio/decode.rs`: symphonia decode to f32 PCM at the graph's sample rate; resample if
  needed.
* `audio/stream.rs`: looping playback stream on a dedicated PipeWire thread.
* `audio/graph.rs`: registry listener that tracks sink nodes and exposes
  `address -> node id` (from `api.bluez5.address`); `link(node)` / `unlink(node)`.
* `audio/volume.rs`: set/ramp node volume via node `Props` (M1 only proved this through
  `wpctl`; confirm the proxy route is not overridden by WirePlumber, and keep the
  re-assert-once safeguard).
* Reuse the registry/link code from `examples/spike_pw.rs` (match ports by `node.id`,
  `port.direction`, `audio.channel`); the spike polls on a timer, the engine should link
  from registry events instead.
* `AudioEngine` handle: `AudioCommand::{Link, Unlink, SetVolume}` in,
  `AudioEvent::{SinkAppeared, SinkRemoved}` out.
* Dev CLI: `bt-roam-player` plays a file to chosen node names, for testing without BT logic.

**Done when:** a file loops gaplessly and can be linked/unlinked to any sink live, with
volume set.

**Outcome / deviations:**
* Implemented as planned, plus a `play` dev subcommand (`--sink`, `--address`, `--volume`,
  `--ramp`, `--unlink-after`, `--seconds`). Verified on a null sink and on the JBL (by ear:
  clean loop, no click, fade-in, silence on unlink). 22 unit tests.
* **No resampler**: the clip stays at its native rate; PipeWire resamples per sink.
* `Link` is persistent intent: it links when the sink (and all four ports) exist and
  re-links if the sink returns, until `Unlink`. Added `Linked`/`Unlinked` events.
* **Volume goes through the device `Route`, not node `Props`** (see DESIGN decision 5 and
  row 18): on a BT sink the node write multiplies with WirePlumber's restored route volume.
  Linking waits for the first volume write to avoid a click.
* Registry globals lack `api.bluez5.address`; the address is parsed from the node name.
* **Not verified**: sink disappearing and returning while wanted (needs a power cycle; do
  in M6/M7 with the real manager), multi-speaker behaviour, `cargo miri` (FFI-heavy).

## M3: Proximity logic (pure code)

* `proximity.rs`: time-based EMA, hysteresis with connect/disconnect dwell, stale-sample
  timeout. Driven by explicit timestamps, no I/O.
* Two scales (M1): true dBm from discovery, and relative dB-below-golden-range from mgmt
  (0 = ideal, ~3 s update period). Thresholds, `tau` and dwell times are configured per
  scale; test with a synthetic trace shaped like the M1 walk (`0, -5, -11, -20, -24, -27,
  ..., -8, 0` in ~3 s steps). Also covers the re-accept cool-down after a proximity drop.
* Property-style tests with synthetic noisy RSSI traces (boundary jitter must not flap).

**Done when:** tests prove no flapping on a jittery boundary trace and correct timing of
both transitions.

## M4: Bluetooth manager

* `bluetooth/adapter.rs`: power on adapter, **set `Pairable = true`**, duty-cycled
  discovery, stream of device property changes normalised to `BtEvent`.
* `bluetooth/rssi.rs`: `RssiSource` trait, `DiscoveryRssi`, and `MgmtRssi` (per M1 result).
* `bluetooth/device.rs`: connect, disconnect, trusted/paired checks, A2DP UUID check,
  `Connected` change events. Map BlueZ errors (`InProgress`, `AlreadyConnected`,
  `Failed`, ...) to a typed error.
* `MgmtRssi` implemented on the raw mgmt socket (`Get Connection Information`), behind a
  `CAP_NET_ADMIN` check with a clear warning and fallback. Unit-test the mgmt response
  parsing; the live read is an `#[ignore]`d hardware test.
* Candidate filter (A2DP Sink UUID / audio Class of Device **including the minor class**,
  plus `allow`/`deny`), as a pure function with unit tests. Use the real classes from M1 as
  fixtures: accept `0x200414`, `0x2c0414`, `0x240404`; reject TVs `0x0c043c`, `0x08043c`.
* Presence for paired speakers: they reconnect on their own, so treat `Connected=true` as
  arrival. Probe-connect (~5 s block) stays an optional, rate-limited fallback, not the
  default.
* Map `br-connection-key-missing` and `br-connection-page-timeout` (seen in M1) to typed
  errors.
* Dev CLI `list`: prints nearby and known audio candidates with smoothed RSSI and
  paired/connected status.

**Done when:** `list` shows live candidates and RSSI for speakers around, including once
connected (if M1 found it feasible), and ignores non-audio devices.

## M5: Auto-pairing agent

* `bluetooth/agent.rs`: `Agent1` implementation (SSP accept, configurable legacy PIN) that
  accepts only for candidates passing the filter and not on `deny`.
* Pairing flow: pair, set `Trusted`, verify **`Bonded`** (not just `Paired`), then hand over
  to the connect step. On `br-connection-key-missing` remove the device and re-pair.
  Honour `auto_pair = false`.
* `forget <address>` subcommand to remove a paired speaker.

**Done when:** a speaker put in pairing mode near the host is paired and trusted without
interaction, a non-audio device in pairing mode is refused, and the speaker reconnects
later.

## M6: Speaker state machine and orchestrator

* `speaker.rs`: states and transitions from DESIGN.md as a pure function
  `(State, Event, Instant) -> (State, Vec<Action>)`. Table-driven tests for every
  transition, including failure/backoff and external disconnects.
* `orchestrator.rs`: creates an actor per newly seen candidate (and drops it after a long
  absence), fans events in from BT and audio, executes actions (connect, link, set volume, unlink, disconnect), enforces
  `max_connected` (keep the strongest).
* Wire `run` subcommand end to end; graceful shutdown on SIGINT/SIGTERM (unlink, optional
  disconnect, stop stream).

**Done when:** walking toward/away from a real speaker connects, links, sets volume, then
unlinks and disconnects, while the loop keeps playing.

## M7: Hardening

* Handle `bluetoothd` or PipeWire restarts (reconnect to bus/daemon, rebuild state).
* Multi-speaker soak test (2-3 speakers, hours), check for leaks and stuck states. Also
  re-test what M1 could only check with one speaker: glitches while discovery runs, and
  whether a probe-connect disturbs other active streams.
* README/NixOS: document the `CAP_NET_ADMIN` route (`security.wrappers`, `setcap` on a copy
  outside the Nix store) and the `Pairable` issue.
* Logging review (state transitions at `info`, raw RSSI at `trace`), clear startup errors
  for missing permissions, absent adapter.
* Docs: README with setup (`setcap`, auto-pairing behaviour and its security implications,
  config example), systemd user unit.

## Later / out of scope for v1

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
