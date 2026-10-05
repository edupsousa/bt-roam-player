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

## M3: Proximity logic (pure code) [DONE 2026-10-03]

* `proximity.rs`: time-based EMA, hysteresis with connect/disconnect dwell, stale-sample
  timeout. Driven by explicit timestamps, no I/O.
* Two scales (M1): true dBm from discovery, and relative dB-below-golden-range from mgmt
  (0 = ideal, ~3 s update period). Thresholds, `tau` and dwell times are configured per
  scale; test with a synthetic trace shaped like the M1 walk (`0, -5, -11, -20, -24, -27,
  ..., -8, 0` in ~3 s steps). Also covers the re-accept cool-down after a proximity drop.
* Property-style tests with synthetic noisy RSSI traces (boundary jitter must not flap).

**Done when:** tests prove no flapping on a jittery boundary trace and correct timing of
both transitions.

**Outcome / deviations:** Built as planned, 17 unit tests in `src/proximity.rs`, no hardware.
* One `Tracker` per speaker and scale (higher = closer), driven by `sample(now, v)` and
  `tick(now)` with caller-supplied `Duration` timestamps. `Params::dbm` and `Params::mgmt`
  build the two parameter sets from config; the owner picks the tracker by RSSI source.
* Config grew `proximity.cooldown_secs` (default 30) and a `[proximity.mgmt]` table
  (`connect_db -10`, `disconnect_db -25`, dwells 3 s / 6 s, tau 5 s). `stale_after_secs` and
  the cool-down are shared by both scales. Per-device overrides still cover dBm only.
* The cool-down starts automatically on every transition to `Far`; `start_cooldown` and
  `reset` exist for external disconnects. A stale timeout forgets the EMA, so the next sample
  re-seeds it. Jitter tests use a built-in LCG instead of a `rand` dependency.
* Not covered here: per-device override plumbing and choosing which tracker applies to which
  speaker; that belongs to M4/M6.

## M4: Bluetooth manager [DONE 2026-10-03]

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

**Outcome / deviations:** Built as planned, 20 new unit tests plus an `#[ignore]`d live test.
* Layout: `bluetooth/{candidate,mgmt,device,adapter,rssi}.rs` and `src/list.rs`. Pure parts
  (filter, mgmt frame encode/parse, error mapping) are unit-tested; `libc` and `futures`
  became normal dependencies, and `thiserror` is now used.
* Verified on hardware: `list` hides non-audio devices and shows the three known speakers.
  With the JBL connected, `sudo list` shows link RSSI `0.0`, and the ignored live test
  returns `rssi 0, tx 12/12`. Without `CAP_NET_ADMIN`, `list` warns once and leaves the link
  column empty. (`sudo` needs `env LD_LIBRARY_PATH=...` here because the nix devShell
  libraries are not inherited.)
* Candidate filter also accepts minor class `0x01` (wearable headset): the WF-1000XM5
  (`0x240404`) reports it, so the planned fixture would otherwise have been rejected.
* bluer does not expose `Device1.Bonded`; checking it moves to M5 (raw D-Bus read).
* Not exercised: probe-connect fallback (not implemented, stays optional), duty-cycled
  discovery timing beyond a 12 s run, `Removed` events, and the mgmt reading changing with
  distance (already seen in M1).

## M5: Auto-pairing agent [DONE 2026-10-03]

* `bluetooth/agent.rs`: `Agent1` implementation (SSP accept, configurable legacy PIN) that
  accepts only for candidates passing the filter and not on `deny`.
* Pairing flow: pair, set `Trusted`, verify **`Bonded`** (not just `Paired`), then hand over
  to the connect step. On `br-connection-key-missing` remove the device and re-pair.
  Honour `auto_pair = false`.
* `forget <address>` subcommand to remove a paired speaker.

**Done when:** a speaker put in pairing mode near the host is paired and trusted without
interaction, a non-audio device in pairing mode is refused, and the speaker reconnects
later.

**Outcome / deviations:** Built as planned (`bluetooth/pairing.rs`, `src/pair.rs`, dev command
`pair [-s SECS]`, real `forget`). Verified on hardware:
* JBL forgotten, put in pairing mode: `pair` paired, trusted and bonded it with no
  interaction, and the adapter had `Pairable: no` beforehand (the app turns it on at start and
  again before each pairing). After connecting once and power-cycling, it reconnected by
  itself. An MX Master 3S in pairing mode was ignored (`NotAudio`) and not paired; the TV and
  watch were ignored too.
* `Device1.Bonded` is read through the `dbus` crate (bluer lacks it). `Paired` without
  `Bonded` removes the device and returns `NotBonded`; the caller pairs again once the device
  is seen in pairing mode again. The agent capability is derived by bluer from the callbacks
  we set, so it is `KeyboardDisplay` rather than `NoInputNoOutput`; all requests are vetted
  with the candidate filter.
* **Finding:** the first `pair` attempt failed once with a page timeout, and later
  `connect()` calls failed with `br-connection-page-timeout` while discovery was off, even
  though the speaker was on and in pairing mode. The same connect succeeded while discovery
  was running. M6 should connect right after a discovery burst that has seen the speaker.
  A speaker paired but never connected does not reconnect by itself after a power cycle;
  after one connection it does.
* Incoming request from a non-audio device: a phone paired to the laptop showed a key, and the
  agent logged `confirmation: rejecting (NotAudio)`; the phone was not paired.
* Not exercised: legacy-PIN speakers.

## M6: Speaker state machine and orchestrator [DONE 2026-10-03]

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

**Outcome / deviations:** `speaker.rs` is a `Machine` with `handle(Event, now) -> Vec<Action>`
(31 table-driven tests: pairing, backoff 2 s to 60 s, awaiting-sink timeout, external
disconnects, sink vanishing and returning, release in every state). Proximity and
`max_connected` are decided outside it: the orchestrator sends `Want(bool)`.
`orchestrator.rs` is one task owning a map of speakers, not one actor each; slow calls (pair,
connect, disconnect, link RSSI) are spawned and report back. `run` is wired, with SIGINT/SIGTERM shutdown (unlink, 0.7 s fade, disconnect what we hold).
* Per speaker two trackers: discovery dBm while not connected, mgmt link dB while connected
  (switched on `Connected`; a speaker we connected starts `Near`, one that connected by itself
  must prove it). Per-device `connect_rssi`/`disconnect_rssi`/`volume` overrides are applied.
  Without `CAP_NET_ADMIN` there is no link RSSI: connected speakers are never released by
  proximity, and self-connected ones are adopted unchecked (warned once).
* `Connect` waits (up to 40 s) for the discovery duty cycle to be on (M5 finding).
* `max_connected` deviation: nearest-first admission, but no pre-emption. A full set is not
  swapped for a stronger newcomer, because dBm and link dB are not comparable. Left for M7.
* **Engine bug found on hardware and fixed:** after a speaker reconnected, its device Route
  never arrived, so the engine waited for the first volume write forever and never linked
  (silence). Fix: re-request the Route every 0.5 s, link anyway after 2 s, and drop stale
  device proxies on removal.
* Verified on the JBL (`run`, under sudo with `XDG_RUNTIME_DIR` and `LD_LIBRARY_PATH` kept):
  adopted a self-connected speaker, linked at 60 %, walking away released it (unlink,
  disconnect, loop keeps playing), coming back it reconnected by itself (about 50 s after our
  disconnect) and was re-linked within 2 s; power-cycle handled as an external drop.
* Not exercised on hardware: `Connect` issued by `run`, pairing inside `run`, more than one
  speaker, `max_connected`. A speaker that reconnects by itself right after an external drop
  is re-adopted without the dBm cool-down (the cool-down only covers the dBm tracker).

## M7: Hardening [DONE 2026-10-03]

* Handle `bluetoothd` or PipeWire restarts. **Done:** the process exits non-zero (see below).
* Multi-speaker soak test (2-3 speakers, hours), check for leaks and stuck states. Also
  re-test what M1 could only check with one speaker: glitches while discovery runs, and
  whether a probe-connect disturbs other active streams. **Done for 10 minutes with 2 speakers
  and 90 s with 3 (below); a multi-hour soak is still open.**
* README/NixOS: `CAP_NET_ADMIN` route and the `Pairable` issue. **Done** (`docs/USAGE.md`).
* Logging review, clear startup errors. **Done.**
* Docs: README, systemd user unit. **Done** (`docs/USAGE.md`, `contrib/bt-roam-player.service`).

**Outcome so far:**
* bluetoothd or PipeWire going away (tested with `systemctl restart bluetooth` and `systemctl
  --user restart pipewire` during `run`): the player logs an error, unlinks and exits non-zero;
  a supervisor (`Restart=on-failure`) restarts it with fresh state. No in-process reconnect.
* **Gap found and fixed (probe-connect):** a paired speaker that is on but disconnected is not in
  discovery results, so it never got an RSSI and was never connected. Now probed with a connect
  after 45 s without RSSI (one at a time, every 30 s at most, only with a free slot); link RSSI
  gets 15 s to confirm it is near, else it is released and left for 60 s. Verified on the JBL.
* **Pairing from `run` verified** (JBL, then XKL-Q5): pairs in about 5 s once discovery is on.
  `Connect`/`Pair` now hold their own discovery session (the duty-cycle window was ending
  mid-attempt, giving page timeouts). Both speakers dropped the link a few seconds after
  pairing, so a drop within 60 s of pairing is retried after 3 s (was about 60 s).
* **Multi-speaker verified** (JBL + XKL-Q5): both paired, connected and played within 15 s
  with `max_connected = 3`; with `max_connected = 1` only the JBL played and the other waited
  for a free slot. No pre-emption (dBm and link dB cannot be compared); still open as an idea.
* `run` prints one readable line per notable change (found, pairing, paired, connecting,
  playing, moved away, disconnected, lost the connection, failures with the retry delay) and a
  one-line summary of who is playing every 30 s (`report.rs`, unit-tested); the raw state machine
  moved to `-v`. A status socket/subcommand was tried and dropped in favour of this.
* On exit `run` prints a session summary to stderr (`session.rs`, unit-tested): speakers seen,
  speakers played on, failed pair/connect attempts, total and "at least one" playing time, and
  connections and playing time per speaker. A connection counts when a speaker reaches `Linked`.
* **Soak, 10 minutes, JBL + XKL-Q5, `twinkle_star.mp3` at volume 0.5, walk-away of the JBL and
  power-cycle of the XKL:** no warnings or errors, no flapping; RSS flat at about 159 MB, 13
  threads, 31-34 descriptors from start to end. Found and fixed a real bug: link RSSI is read
  through one shared mgmt socket, and replies were matched by opcode only, so after one read
  timed out (the controller is busy while it pages an absent speaker) every later read took
  the previous reply, failed as "malformed", and both speakers were released as far away
  after 30 s. Replies for another address are now skipped (unit test added).
* **Never-paired speaker found and used (3 speakers):** with no `allow` list and only the Echo and
  earbuds denied, a Shokz OpenRun Pro 2 in pairing mode was discovered, paired in 5 s, dropped
  the link as the others do, was probed back and linked about 18 s after the run started; the
  XKL linked too (the JBL, see next). A 30 s run is too short: the single 10 s discovery window
  may miss a new device (it did once), so the new-device test needs at least 60 s.
* **3 speakers at once** (JBL, XKL-Q5, Shokz; Echo and earbuds denied; `max_connected = 3`): all
  three seen at startup and linked within 7 s, steady for 90 s, no warnings or flapping.
* **Bug found and fixed:** at startup, reading a device intermittently failed with D-Bus
  "Failed to send message" (a different device each run: the MX, the Echo, the JBL). A device
  whose first read is lost stays unknown until a property changes, so a paired speaker was
  ignored until it was power-cycled. Reads and the event subscription are now retried (6
  times, 0.3 s apart). Not reproduced after the fix in three scans, but the failure was
  intermittent, so this is unproven.
* **Finding (measured, `btmon`):** when the XKL was powered off, the JBL crackled (and once went
  silent for about 15 s) while our stream and link never changed state. A `btmon` capture shows
  the XKL stopped acknowledging packets at the power-off and the controller kept the dead link
  for the 20 s default link supervision timeout (`Connection Timeout (0x08)`). Over those 20 s
  the JBL's ACL packets took about 57 ms each, against 7-10 ms before and 7 ms after. A speaker
  that disconnects cleanly (the JBL power-off, `Remote User Terminated`) causes nothing. It is the
  controller wasting airtime on the dead link, so the player cannot avoid it; a shorter BR/EDR
  link supervision timeout would limit it. **To try:** BlueZ `main.conf` `[BR] LinkSupervisionTimeout`
  (NixOS: `hardware.bluetooth.settings.BR.LinkSupervisionTimeout`, 0.625 ms units, default
  0x7D00 = 20 s), or the mgmt "Set Default System Configuration" command (0x004a) from the
  player; check with `btmon` whether the kernel applies it to the links. Probing less often
  while others play is a smaller, separate mitigation.

## Post-M7 changes

* **`--adapter hciN` (2026-10-04):** global CLI option to choose the Bluetooth adapter for
  `run`, `list`, `pair` and `forget`; a missing name fails listing the available ones, and the
  adapter in use is logged at startup. Documented in DESIGN.md (decision 1) and USAGE.md.
* **Mgmt socket `ENOMEM` investigation (2026-10-05):** a run logged
  `mgmt rssi: management socket: Cannot allocate memory (os error 12)` every 3 s for one
  connected speaker, stopping when that speaker went away (the next line was an unrelated
  probe). The failing syscall was not identified (the error was not labelled). Hypotheses:
  (1) a kernel allocation failure while the link is being torn down; (2) the unread broadcast
  events filling the socket's receive buffer. **Changes:** `connection_info` drains the socket
  before each request, and I/O errors name the step (`drain`/`send`/`recv`/`set timeout`).
  Unit tests and clippy pass; **not verified on hardware**.
  **To do:** rerun the roaming scenario with `RUST_LOG=trace`. A `send:` error points at the
  kernel command side, `recv:` at reply delivery, no error means the backlog was the cause
  (the `trace` line with the drained frame count would confirm). If it persists, treat
  `ENOMEM` as "no sample" at lower log level and reopen the socket after repeated failures.

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
