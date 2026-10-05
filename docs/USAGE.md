# bt-roam-player: usage and behaviour

Plays one looping audio file and routes it to every nearby Bluetooth speaker. Speakers are
paired, connected and linked when you get close to them and released when you walk away, while
the loop keeps playing. Linux only: BlueZ and PipeWire.

```
bt-roam-player run --file loop.flac        # the player
bt-roam-player list [-s SECS] [-a]         # speakers seen, RSSI, verdict
bt-roam-player pair [-s SECS]              # pair every speaker in pairing mode
bt-roam-player forget AA:BB:CC:DD:EE:FF    # remove a paired speaker
```

Flags: `-c PATH` config file, `--adapter hciN` Bluetooth adapter to use (default: BlueZ's
default adapter; the one in use is logged at startup; bonds are per adapter, so speakers must
be paired on the adapter you select), `-v`/`-vv` log level (or `RUST_LOG`). Configuration is
optional, see `../config.example.toml`.

## Build

`cargo build --release`. Needs the PipeWire, libspa, D-Bus and BlueZ development headers
(the Nix devShell in the parent workspace provides them).

## How it behaves

* A speaker is **wanted** when its smoothed RSSI stays above `connect_rssi` for the dwell time,
  and released when it stays below `disconnect_rssi`. Up to `max_connected` speakers are held,
  nearest first. A newcomer does not push out a speaker already held.
* While connected, proximity comes from the controller's link RSSI (relative dB, needs
  `CAP_NET_ADMIN`); before connecting, from discovery RSSI (dBm).
* Speakers connect on their own after a first connection; the player adopts them when they are
  near and releases them when they are far.
* A paired speaker that is on but disconnected gives no discovery RSSI. After 45 s without any,
  and while a slot is free, the player probes it with a connect (one at a time, at most every
  30 s). If the page times out it is left alone; if it connects, its link RSSI has 15 s to show
  it is near, otherwise it is released again.
* A speaker that drops the link within a minute of being paired (several do) is reconnected
  after a few seconds instead of waiting out the cool-down.
* Pair and connect attempts hold their own discovery session, because they fail with a page
  timeout when discovery is off.
* Each speaker's volume is set to `default_volume` (or the per-device `volume`) with a short
  ramp before the stream is linked, so there is no click.
* If bluetoothd or PipeWire goes away the process exits with an error. Run it under a
  supervisor (see `../contrib/bt-roam-player.service`) and it restarts with fresh state.

## What `run` prints

One line per notable change, with the speaker's name and address, and a one-line summary every
30 s:

```
JBL GO 2 [00:11:22:33:44:55]: found (paired)
JBL GO 2 [00:11:22:33:44:55]: connecting
JBL GO 2 [00:11:22:33:44:55]: playing
XKL-Q5 [66:77:88:99:AA:BB]: lost the connection (powered off or out of range)
JBL GO 2 [00:11:22:33:44:55]: moved away, releasing
JBL GO 2 [00:11:22:33:44:55]: disconnected
status: playing on 1 of 3: JBL GO 2; others: XKL-Q5 (idle)
```

The changes covered are found, pairing, paired, connecting, playing, moved away (released),
disconnected, lost the connection, could not connect or pair (with the retry delay), and
waiting for a free slot. `-v` adds the raw state machine, links and discovery windows, `-vv`
raw RSSI.

## Session summary

When `run` stops (Ctrl-C, SIGTERM, or losing bluetoothd or PipeWire) it prints a summary to
stderr, after the speakers have been released:

```
Session summary (12m 40s)
  speakers seen: 3   played on: 2   failed attempts: 1
  total playing time: 19m 05s across 2 speakers; at least one speaker playing: 15m 00s
    JBL GO 2 [00:11:22:33:44:55]  3 connections  11m 20s
    XKL-Q5 [66:77:88:99:AA:BB]  1 connection  7m 45s
    Shokz [CC:DD:EE:FF:00:11]  seen, never played
```

"Seen" counts audio speakers that passed the allow/deny filters. A connection counts once the
speaker reaches the playing state. "Total playing time" adds up every speaker, so it exceeds
the session length when several play at once; the "at least one" figure does not. Failed
attempts are pairing and connect failures.

## Link RSSI permission (`CAP_NET_ADMIN`)

Reading the RSSI of a live connection uses the kernel management socket and needs
`CAP_NET_ADMIN`. Without it the player still works, but warns once at start: speakers are never
released by proximity, and ones that connect on their own are adopted without a check.

* **Any distro:** copy the binary out of any read-only store and grant the capability:
  `install -m755 target/release/bt-roam-player ~/.local/bin/ &&
  sudo setcap cap_net_admin+ep ~/.local/bin/bt-roam-player`. Redo it after every rebuild.
* **NixOS:** the Nix store cannot hold capabilities. Use a wrapper:
  ```nix
  security.wrappers.bt-roam-player = {
    source = "/path/to/bt-roam-player";
    capabilities = "cap_net_admin+ep";
    owner = "root"; group = "root";
  };
  ```
  and run `/run/wrappers/bin/bt-roam-player`. For a quick test, `sudo` works if you keep the
  environment: `sudo env "LD_LIBRARY_PATH=$LD_LIBRARY_PATH" "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" ...`.
  Under `sudo`, PipeWire is reached through `XDG_RUNTIME_DIR`.

With `-v`, a failed link RSSI read is logged as `mgmt rssi: management socket: <step>: <error>`,
where the step (`drain`, `send`, `recv`) says which socket operation failed. Before each read
the player empties the socket's queue of unrelated kernel events. A read that fails just counts
as no sample, so the speaker is released only if the readings stay missing past the stale time.

## Pairing and its security implications

With `auto_pair = true` (default) the player registers a BlueZ pairing agent that accepts
pairing from **audio output devices only** (headphones, speakers, car audio; TVs and other
classes are rejected) and sets them trusted. Anything that looks like an audio sink and is in
pairing mode nearby can be paired while the player runs. Set `auto_pair = false` to use only
speakers you paired yourself, or restrict with `allow`/`deny` (address or name patterns).
Legacy-PIN speakers get `pin` (default `0000`).

## Known issues

* BlueZ sometimes has `Pairable: no` on the adapter, which makes pairing succeed without
  bonding. The player turns it on at start and before each pairing; if you pair by hand run
  `bluetoothctl pairable on` first.
* `Connect` and `Pair` only work while discovery is running (page timeouts otherwise), so the
  player keeps a discovery session open during each attempt.
* With two or more speakers on one adapter, a speaker that vanishes without disconnecting
  (powered off, out of range) degrades the others for up to 20 s: the controller keeps
  retrying the dead link until the link supervision timeout, and the other speakers' packets
  take 5-8 times longer. Measured with `btmon` (JBL GO 2 and XKL-Q5). A shorter BlueZ
  `LinkSupervisionTimeout` (`[BR]` section of `main.conf`) is the likely mitigation, untested.
* A speaker paired but never connected does not reconnect by itself after power-cycling; connect
  it once.
* Link RSSI is relative (0 = ideal), discovery RSSI is true dBm; they are not compared with each
  other, which is why there is no pre-emption when `max_connected` is reached.
