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
puts `wirebond` on PATH (on Windows it also adds a firewall rule for the executable). No traffic is
captured or rerouted until you give the device a role: until then the daemon only watches the
device's links (a UDP socket on each, and a STUN request and a captive-portal check when one
appears) and listens on its mesh address. Install it on the client and on the anchor; a phone used
as a support needs nothing. From then on the daemon updates itself from GitHub releases, checking
each download's hash and rolling back a version that does not stay healthy; running the installer
again also updates.

`sudo wirebond uninstall` stops the daemon, which undoes what its role set up on the network, and
removes the service, the executable, its PATH entry and its settings. A support's sharing is the
exception: turn it off first with `wirebond support off`.

Linux needs systemd (the installer stops without it), `ip` and `nft` (the iproute2 and nftables
packages), `curl` for the daemon's self-update (the installer can download with `wget`; the daemon
cannot), and `nmcli` (NetworkManager) for `support`. With systemd-resolved a client's DNS goes to
the anchor; without it DNS stays with the local resolver, and status says `dns_local`.

<details>
<summary>Without the installer</summary>

Download your machine's asset and `SHA256SUMS` from the
[latest release](https://github.com/TristanCopley/wirebond/releases/latest) into one folder. There,
check the hash, make the file executable (a download is not) and install:

```sh
grep " wirebond-Linux-x86_64$" SHA256SUMS | sha256sum -c -    # on macOS: shasum -a 256 -c -
chmod +x wirebond-Linux-x86_64
sudo ./wirebond-Linux-x86_64 install
```

On Windows, in an administrator PowerShell, compare `(Get-FileHash .\wirebond-Windows-AMD64.exe).Hash`
with the asset's line in `SHA256SUMS`, then run `.\wirebond-Windows-AMD64.exe install`. The assets
are named as `uname -sm` prints the machine: `wirebond-Linux-x86_64`, `-aarch64`, `-armv7l`,
`-armv6l` (static), `wirebond-Darwin-arm64`, `-x86_64`, `wirebond-Windows-AMD64.exe`, `-ARM64.exe`
(Wintun inside).

A file a browser downloaded on macOS is also quarantined: `xattr -d com.apple.quarantine
wirebond-Darwin-arm64` lets it run (the installer's `curl` download is not). From 2.0.0-rc.2 on,
each executable carries a signed record of the commit and the workflow run that built it, which
GitHub's CLI checks: `gh attestation verify wirebond-Linux-x86_64 --repo TristanCopley/wirebond`.

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

3. **On the client**, with the phone plugged in over USB and its tethering (Personal Hotspot) on.
   On a Mac, first turn off "Disable unless needed" under System Settings > Network > iPhone USB:
   while it is on, macOS leaves the phone unused as long as Wi-Fi works, and the bond has one link.

   ```sh
   sudo wirebond client xanth             # the anchor's name on the mesh
   wirebond                               # status: links, lanes, delay, loss, what each lane carries
   ```

4. **Moonlight:** on the client, add the host at the anchor's bond address, **10.77.0.1**
   (**192.168.137.1** for a Windows anchor, whose Internet Connection Sharing fixes that subnet).
   `wirebond status` prints that line while the bond is up. Sunshine's ports need no forwarding.
   Not the anchor's mesh address: traffic to that stays on the mesh and is not bonded.

Commands that change something need `sudo`, or an administrator PowerShell on Windows, where the
commands are the same without `sudo`. Reading the status needs nothing. `sudo wirebond client off`
stops the bond, and traffic goes direct again. Fedora and RHEL leave `/usr/local/bin` off sudo's
PATH: there, run `sudo /usr/local/bin/wirebond …`.

## Streaming a game with Moonlight and Sunshine

What wirebond was made for: Sunshine on a PC at home, which is the anchor, and Moonlight on a
laptop at a hotel, which is the client, with the hotel's Wi-Fi and a phone on USB as its two links.

**Once, at home**

1. Pair Moonlight with the PC as you normally would. Sunshine needs no change: it listens on every
   address, the bond's included.
2. Keep the mesh up on the PC with nobody signed in: in Tailscale, turn on "Run unattended" and turn
   off the PC's key expiry. Keep the PC itself awake too (on Windows: Settings, System, Power,
   sleep "Never" when plugged in): an anchor asleep is an anchor gone. Note both devices' mesh
   addresses (`tailscale ip -4`): wirebond takes an address wherever it takes a name, and an
   address needs no lookup from a hotel.
3. Forward UDP port 7849 on the home router to the PC. Lanes are UDP between the client's links
   and the anchor, and a phone carrier's address translation is of the kind a home router does not
   let in unasked; with the port forwarded, the phone's lane reaches the anchor whatever the carrier
   does. If the home line has IPv6, let UDP 7849 in to the PC there too.
4. Know what the phone will carry. Performance mode sends the whole stream on every link: about
   13.5 GB an hour at 30 Mbit/s, out of the phone's hotspot allowance. Moonlight's bitrate is the
   lever.

**At the hotel**

1. Join the Wi-Fi and get through its sign-in page first, before the phone is plugged in. Under
   the bond a browser's traffic leaves at the anchor and never meets that page.
2. On the phone: Personal Hotspot on, cable in, "Trust This Computer".
3. On a Mac, check two things in System Settings, Network. "Disable unless needed" on the iPhone USB
   service must be off: with it on, macOS leaves the phone's link idle while Wi-Fi works, and the
   bond has one link. And Wi-Fi stays above iPhone USB in the service order, so that whatever is
   outside the bond prefers Wi-Fi.
4. Start the bond and watch it:

   ```sh
   sudo wirebond client 100.x.y.z        # the anchor's mesh address
   wirebond status --watch
   ```

   A lane through each link is `up` within a few seconds.
5. In Moonlight, add the host at the address on status's `Moonlight:` line. It becomes one more
   address of the PC you paired; nothing is paired again. 1080p at 120 FPS and 20 to 30 Mbit/s is
   a good place to start.

## When it does not work

`wirebond status` lists what the daemon knows to be wrong under `problems`, each with its next
step. The daemon's log is `wirebond.log` in its state directory: `/var/lib/wirebond` on Linux,
`/Library/Application Support/wirebond` on macOS, `%ProgramData%\wirebond` on Windows.

| What you see | Likely cause | What to do |
| --- | --- | --- |
| Status lists one link | The phone's link is not up | Hotspot on, cable in, "Trust"; on a Mac, "Disable unless needed" off |
| The phone's lanes stay `punching` | The anchor's router does not let the carrier in | Forward UDP 7849 to the anchor |
| `captive_portal` on a link | The hotel's sign-in ran out | `sudo wirebond client off`, sign in, start the bond again |
| `mesh_captured`, and capture is removed | The mesh's own packets entered the bond: an exit node is on, or Tailscale restarted | Exit node off, then start the bond again |
| Moonlight cannot reach the host | It is aimed at another address | Use the address on status's `Moonlight:` line. On the client, `curl http://<that address>:47989/serverinfo` prints XML when Sunshine answers |
| The stream stutters | A link is late or losing | `wirebond status --watch` shows each lane's delay, loss and share. Lower Moonlight's bitrate |
| No internet after the bond ended | Capture is still in place | `sudo wirebond client off` |
| The phone's link turned slow | Its hotspot allowance is used up | The carrier's app says |

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

`wirebond help` lists them: `wirebond [status [<device>] [--watch]]` (`--watch` shows it again every
second: lanes, delay, loss and each link's share, live while you play), `client <anchor> [mode] | off`, `mode
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
cargo test -p wirebond-netsim --profile sim   # the simulator's acceptance set (SPEC §23)
sudo tests/system/run.sh                      # the system harness in network namespaces (Linux)
```

Rust 1.99.1, the toolchain CI pins. On Linux the tests run `ip`, so they need iproute2; a Windows
build embeds Wintun, which `.github/scripts/wintun.sh` downloads and checks first. The crates:
`crates/core` (the pure engine), `crates/wirebond` (daemon, OS layer, CLI), `crates/netsim`
(simulator and acceptance), `crates/load` (the harness's load and echo tool). A tag `v*.*.*`
builds, tests and publishes a release (`.github/workflows/release.yml`). A manual run of that
workflow given a tag publishes one too, after the same checks less the simulator's full set: how a
release candidate goes out.
