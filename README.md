# wirebond

wirebond lets a struggling device borrow the networks around it. You run a mesh (Tailscale or
similar) across your devices. When one of them, the **client**, sits on a poor connection such as
hotel Wi-Fi that stalls, wirebond joins that connection with the independent networks cabled to it
(a phone on USB, a router on Ethernet: its **supports**) into one **bond**. The bond ends at another
of your devices with a good connection, the **anchor**. All of the client's traffic travels the bond
and leaves for the internet at the anchor; traffic for the anchor itself, such as a game streamed
with Moonlight, simply ends there.

[SPEC.md](SPEC.md) is the specification; [NOTES.md](NOTES.md) says where the build stands. 2.0 is
a release candidate: it is tested in a network simulator and in network namespaces, and has yet to
run on real hardware.

## Install

On macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/TristanCopley/wirebond/main/install.sh | sh
```

On Windows, in an administrator PowerShell:

```powershell
irm https://raw.githubusercontent.com/TristanCopley/wirebond/main/install.ps1 | iex
```

Either downloads this machine's executable from the latest release, checks it against the
release's `SHA256SUMS`, and runs `wirebond install`, which sets wirebond up as a system service and
puts `wirebond` on PATH. Nothing changes on the network until you give the device a role. Install it
on the client and on the anchor; a phone used as a support needs nothing. From then on the daemon
updates itself from GitHub releases, checking each download's hash and rolling back a version that
does not stay healthy; running the installer again also updates. `sudo wirebond uninstall`
removes everything wirebond created.

Linux needs systemd, `ip` and `nft` (the iproute2 and nftables packages), and `nmcli` for `support`.

<details>
<summary>Without the installer</summary>

Download your machine's asset and `SHA256SUMS` from the
[latest release](https://github.com/TristanCopley/wirebond/releases/latest), check the hash, and
run `sudo ./wirebond-<OS>-<arch> install` (on Windows, `.\wirebond-Windows-AMD64.exe install` in an
administrator PowerShell). The assets are named as `uname -sm` prints the machine:
`wirebond-Linux-x86_64`, `-aarch64`, `-armv7l`, `-armv6l` (static), `wirebond-Darwin-arm64`,
`-x86_64`, `wirebond-Windows-AMD64.exe`, `-ARM64.exe` (Wintun inside).

| OS | Service | Executable |
| --- | --- | --- |
| Linux | `wirebond.service` (systemd) | `/usr/local/bin/wirebond` |
| macOS | LaunchDaemon `com.wirebond` | `/Library/Application Support/wirebond/wirebond`, linked from `/usr/local/bin/wirebond` |
| Windows | service `wirebond` | `C:\Program Files\wirebond\wirebond.exe`, on the system PATH |

</details>

## Set up

1. **Put both devices on your mesh.** With Tailscale, sign both in to the same tailnet; each is
   known by its name there (`tailscale status` lists them) or its mesh address (`100.x.y.z`).
2. **On the anchor**, the device with the good connection (a PC at home, a server):

   ```sh
   sudo wirebond anchor on
   sudo wirebond anchor allow laptop      # the client's name on the mesh
   ```

3. **On the client**, with the phone plugged in over USB and its tethering (Personal Hotspot) on:

   ```sh
   sudo wirebond client xanth             # the anchor's name on the mesh
   wirebond                               # status: links, lanes, delay, loss, each link's share
   ```

4. **Moonlight:** on the client, add the host at the anchor's bond address, **10.77.0.1**
   (**192.168.137.1** for a Windows anchor, whose Internet Connection Sharing fixes that subnet).
   `wirebond status` prints that line while the bond is up. No ports to forward: traffic to the
   anchor's mesh address stays on the mesh and is not bonded.

Commands that change something need `sudo`, or an administrator PowerShell on Windows, where the
commands are the same without `sudo`. Reading the status needs nothing. `sudo wirebond client off`
stops the bond, and traffic goes direct again.

## What it does and does not promise

A bond with lanes through at least two independent networks is never worse than the anchor reached
over the mesh's single path, at any delay percentile in performance mode, and gives more throughput
in bandwidth mode when two or more lanes have spare capacity (SPEC §1, P1). It does not try to beat
the client's own direct connection: leaving at the anchor is a detour you accept. Every device in
the path is yours; there is no third-party relay.

## Roles

A device runs one role at a time:

| Role | What it does |
| --- | --- |
| `client` | Bonds its links to one anchor. All its traffic is captured into the bond while the bond is up; if the bond is lost it fails open and traffic goes direct. |
| `anchor` | Accepts bonds from the devices on its allow-list (at most 8), delivers their traffic to itself or forwards it to the internet with the OS's own NAT. |
| `support` | Shares one of its links over a cable with one client. A phone needs no wirebond: tethering it over USB is enough. |
| `none` | The default. |

## Modes

- **performance** (the default) sends every packet over every independent path and keeps the
  first copy to arrive: the delay you see is the best path's at every moment. The price is data.
- **bandwidth** spreads packets across the paths for more throughput, with more variation in delay.

`sudo wirebond client xanth bandwidth` picks it when the bond starts; `sudo wirebond mode
performance|bandwidth` changes it later.

## Commands

`wirebond help` lists them: `wirebond [status [<device>]]`, `client <anchor> [mode] | off`, `mode
performance|bandwidth`, `pick [<device>...] [--mode M] [--seconds N] [--dry-run]` to measure
candidate anchors and choose the best (`pick --add|--remove <device>` keeps the list), `anchor
on|off`, `anchor allow|deny <device>...`, `support on <uplink> <cable> [--for <client>] | off`,
`update`, `install`, `uninstall` and `version`. Add `--json` for the machine-readable form. Exit
codes: 0 ok, 1 failed, 2 usage, 3 no daemon, 4 needs administrator rights, 5 refused by the peer
(SPEC §19).

## Build and test

```sh
cargo build --release -p wirebond             # the daemon and CLI
cargo test --workspace --exclude wirebond-netsim
cargo test -p wirebond-netsim --release       # the simulator's acceptance set (SPEC §23)
sudo tests/system/run.sh                      # the system harness in network namespaces (Linux)
```

Rust 1.99.1. The crates: `crates/core` (the pure engine), `crates/wirebond` (daemon, OS layer,
CLI), `crates/netsim` (simulator and acceptance), `crates/load` (the harness's load and echo tool).
A tag `v*.*.*` builds, tests and publishes a release (`.github/workflows/release.yml`).
