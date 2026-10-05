# Cross-compiling for Raspberry Pi (64-bit Raspberry Pi OS, Debian 13)

Target: `aarch64-unknown-linux-gnu` (Pi 3 and newer running the 64-bit OS).

## Build host

Needs the Nix devShell (it provides the aarch64 Rust std, unwrapped clang and lld) and
Docker (only used once, to fetch a Debian trixie arm64 sysroot).

```sh
scripts/build-arm64.sh        # creates target/sysroot-arm64 on first run, then builds
scp target/aarch64-unknown-linux-gnu/release/bt-roam-player pi@raspberrypi:
```

`scripts/make-sysroot.sh` downloads the arm64 `-dev` packages (libc, libdbus, libpipewire,
libspa) from Debian trixie, so the binary links against the same libraries and glibc the Pi
runs. To refresh the sysroot, delete `target/sysroot-arm64` and rebuild. Without Nix, any
setup works that provides the arm64 Rust std, clang + lld, and the same environment
variables as `scripts/build-arm64.sh`.

## On the Pi

```sh
sudo apt install pipewire wireplumber libspa-0.2-bluetooth bluez libdbus-1-3
bluetoothctl pairable on
./bt-roam-player
```

The binary links `libpipewire-0.3` and `libdbus-1`. The Lite image does not run
PipeWire by default. Install the packages above and run the player as a user with a PipeWire session
(`loginctl enable-linger $USER` on a headless Pi). Reading RSSI and TX power uses the
Bluetooth management socket, which needs `CAP_NET_ADMIN`:
`sudo setcap cap_net_admin,cap_net_raw+eip ./bt-roam-player`.

The Pi 3's onboard radio shares an antenna with Wi-Fi; a USB dongle plus `--adapter hci1`
is more reliable for A2DP.
