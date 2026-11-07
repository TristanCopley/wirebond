# wirebond

wirebond lets a struggling device borrow the networks around it. You run a mesh (Tailscale or
similar) across your devices. When one of them, the **client**, sits on a poor connection such as
hotel Wi-Fi that stalls, wirebond joins that connection with the independent networks cabled to it
(a phone on USB, a router on Ethernet: its **supports**) into one **bond**. The bond ends at another
of your devices with a good connection, the **anchor**. All of the client's traffic travels the bond
and leaves for the internet at the anchor; traffic for the anchor itself, such as a game streamed
with Moonlight, simply ends there.

2.0 replaces 1.x entirely: no wire, config or CLI compatibility. [SPEC.md](SPEC.md) is the
specification; [NOTES.md](NOTES.md) says where the build stands.

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

## Install

Download your platform's asset from the latest GitHub release and check it against `SHA256SUMS`:
`wirebond-Linux-x86_64`, `-aarch64`, `-armv7l`, `-armv6l` (static), `wirebond-Darwin-arm64`,
`-x86_64`, `wirebond-Windows-AMD64.exe`, `-ARM64.exe` (Wintun inside). Then run any command that
changes something, such as `wirebond anchor on`, with administrator rights once: it installs the
system service (`wirebond.service`, LaunchDaemon `com.wirebond`, or the `wirebond` Windows service)
and the binary (`/usr/local/bin/wirebond` or `%ProgramFiles%\wirebond\wirebond.exe`). Linux needs
`ip` and `nft`, and `nmcli` for `support`. The daemon updates itself from GitHub releases, verifying
the hash and rolling back a version that does not stay healthy. `wirebond uninstall` removes
everything wirebond created.

## Use

On the device with the good connection (a PC at home, a server):

```
wirebond anchor on
wirebond anchor allow laptop
```

On the device on the poor connection, with the phone tethered over USB:

```
wirebond client xanth              # or: wirebond client xanth bandwidth
wirebond                           # status: links, lanes, delay, loss, each link's share
wirebond client off                # stop; traffic goes direct
```

Other commands: `wirebond mode performance|bandwidth`, `wirebond pick [<device>...] [--mode M]
[--seconds N] [--dry-run]` to measure candidate anchors and choose the best, `wirebond pick --add
<device>`, `wirebond support on <uplink> <cable> [--for <client>]`, `wirebond status <device>`,
`wirebond update`, `wirebond version`. Add `--json` for the machine-readable form. Exit codes: 0 ok,
1 failed, 2 usage, 3 no daemon, 4 needs administrator rights, 5 refused by the peer (SPEC §19).

## Moonlight

Streaming from the anchor needs no port setup: in Moonlight on the client, add the host at the
anchor's bond address, **10.77.0.1** (**192.168.137.1** for a Windows anchor, whose Internet
Connection Sharing fixes that subnet). `wirebond status` prints that line while the bond is up.
Traffic to the anchor's mesh address stays on the mesh and is not bonded.

## Build and test

```
cargo build --release -p wirebond             # the daemon and CLI
cargo test --workspace --exclude wirebond-netsim
cargo test -p wirebond-netsim --release       # the simulator's acceptance set (SPEC §23)
sudo tests/system/run.sh                      # the system harness in network namespaces (Linux)
```

Rust 1.99.1. The crates: `crates/core` (the pure engine), `crates/wirebond` (daemon, OS layer,
CLI), `crates/netsim` (simulator and acceptance), `crates/load` (the harness's load and echo tool).
