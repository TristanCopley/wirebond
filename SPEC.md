# wirebond 2.0 specification

Version 2.0.0, November 2026. It replaces 1.x entirely: no wire, config or CLI compatibility. 1.x never ran on real hardware, so nothing migrates.

## Abstract

wirebond lets a struggling device borrow the networks around it. You run a mesh (Tailscale or similar) across your devices. When one of them, the **client**, sits on a poor connection, such as hotel Wi-Fi that stalls, wirebond joins that connection with the independent networks cabled to it (a phone on USB, a router on Ethernet: its **supports**) into one **bond**. The bond ends at another of your devices with a good connection: the **anchor**. All of the client's traffic travels the bond and leaves for the internet at the anchor. Traffic for the anchor itself, such as a game streamed from your PC with Moonlight, simply ends there.

A bond runs in one of two modes. **Performance**, the default, sends every packet over every independent path and keeps the first copy to arrive. The delay you see is the best path's at every moment, so a spike on one network does not reach you; the price is data. **Bandwidth** spreads packets across the paths for more throughput, at the cost of more variation in delay. The promise is narrow: a bond beats the same traffic relayed through the same anchor over one path. It does not try to beat the client's own direct connection to the internet; the detour through the anchor is the price.

## 1. Goals, non-goals and the promise

**Goals**

| | Goal |
| --- | --- |
| G1 | Steadier delay (performance) or more throughput (bandwidth) for all of a client's traffic, using independent networks cabled to it. |
| G2 | Nothing to configure per port, app or flow: everything the client sends is bonded. |
| G3 | Every device in the path is the user's own. No third-party relay. |
| G4 | Small, fast Rust with few dependencies: one static binary per platform. |
| G5 | Honest numbers: each device shows what it contributes and what the bond achieves. |

**Non-goals**

| | Non-goal | Why |
| --- | --- | --- |
| N1 | Beating the client's direct internet connection | Leaving at the anchor adds a detour. The user accepts it. |
| N2 | Using the client's raw direct path alongside the bond: racing, per-connection balancing | A server answers one address, so this cannot help inside a connection, and it splits the user's identity across two addresses. |
| N3 | Wireless supports | A wireless hop to a support shares the client's radio, which is often the bottleneck. |
| N4 | Data caps, per-flow modes, background anchor selection | The user buys steadiness with data on purpose. A mode per bond is enough. Anchor selection is a command (§18). |
| N5 | Replacing the mesh | Identity, trust, addressing and key transport stay the mesh's. wirebond never calls the mesh's API or CLI. |
| N6 | Phones as clients or anchors | A phone takes part only as a tethered support. |

**The promise (P1).** Compare the same client, anchor and traffic with the **baseline**: the anchor reached over the mesh's single path. A bond with lanes in at least two bottleneck groups is never worse than the baseline at any delay percentile in performance mode. In bandwidth mode it delivers more throughput whenever two or more lanes have spare capacity. The acceptance tests (§23) measure P1 directly.

## 2. Terms

| Term | Meaning |
| --- | --- |
| Mesh | The overlay that gives each device an identity, trust and one address (Tailscale: 100.64.0.0/10 and fd7a:115c:a1e0::/48). |
| Node | A device running the wirebond daemon. |
| Client | A node whose traffic is bonded. |
| Anchor | A node where bonds end. Bonded traffic leaves it for the internet, or is delivered to the anchor itself. |
| Support | A device cabled to a client that gives it an independent network: a phone on USB (nothing runs on the phone), or a computer or router on Ethernet or USB sharing its own uplink. |
| Link | One network connection of a node: Wi-Fi, Ethernet, a tethered phone. |
| Bottleneck group | Links that share a choke point: the same public address or the same gateway. A copy helps only when it goes through a different group. |
| Lane | One encrypted UDP path from one client link to one anchor link, plus one lane over the mesh between the two mesh addresses. |
| Bond | One client, one anchor, their lanes and a mode. |
| Bond addresses | The private addresses the anchor gives the bond: the anchor is 10.77.0.1 and fd77:b0d::1; each client gets 10.77.0.N and fd77:b0d::N. A Windows anchor uses 192.168.137.0/24, which ICS sets (§6.2). |
| Route | The bond, plus the anchor's leg to the destination. |
| Flow | One conversation, keyed by protocol, source and destination address, and source and destination port (§13). |
| Small flow | A flow direction under 1 Mbps over 200 ms: input, voice, control. |
| Bulk flow | Any other flow: video, downloads. |

## 3. Roles and the bond lifecycle

### 3.1 Roles

A node has exactly one role at a time: `client`, `anchor`, `support` or `none`. The role is set with the CLI (§19), stored in `config.json` (§22.4) and applied at once.

- **Client.** It has one anchor, set by `wirebond client <anchor>` or by `wirebond pick` (§18). All of its traffic is captured (§5) while its bond is up.
- **Anchor.** It accepts bonds from the devices on its allow-list (`wirebond anchor allow <device>`; empty by default). It serves at most `MAX_BONDS` (8) clients. It delivers bonded traffic locally or forwards it to the internet (§6).
- **Support.** It shares one of its links over a cable to one client (§8). A phone needs no wirebond; it is a support by being tethered.

### 3.2 Bond lifecycle

1. The client resolves the anchor's name through the system resolver (MagicDNS) or takes a literal mesh address. It dials TCP `PORT` (7849) on the anchor's mesh address.
2. Both sides send `hello` (§16). A major-version mismatch, or a client not on the allow-list, gets `error` and the channel closes.
3. The client sends `bond` with `op: "open"` and its mode. The anchor answers `bond` with `op: "ok"`: the client's bond addresses, the MTU, the DNS address and the anchor's egress (`v4`, `v6`, both or none).
4. Each side sends `key` for its sending direction (§10.2) and `links` with its candidates (§9).
5. Punching and path validation (§9.4) bring lanes up. The mesh lane comes up first, because the mesh already connects the two addresses.
6. Once at least one lane is validated, the client installs capture (§5.4) and traffic flows.
7. Teardown (`wirebond client off`, a role change or daemon exit): the client removes capture first, then sends `bond` with `op: "close"`, then closes the channel. The anchor frees the bond's state within 1 s.

An anchor keeps each client's bond addresses for as long as the client stays on its allow-list, so a reconnect keeps them. NAT state on the anchor does not survive a reconnect, and open connections through the internet break.

## 4. Architecture

### 4.1 Processes

- **`wirebond run`**: the daemon, one per device, run as a system service with administrator rights. Capture, routes, forwarding and NAT need them.
- **`wirebond <verb>`**: the CLI. It talks to the local daemon over the local API (§20.1).
- **`wirebond-view`**: the optional viewer app (§20.4). It is a separate binary and only reads.

### 4.2 Crates

| Crate | Contents | Rule |
| --- | --- | --- |
| `crates/core` (`wirebond-core`) | keys, ratchet, wire format, replay windows, probes, estimates, liveness, rate control, pacer, flows, both modes, reorder, stats counters | Pure: no I/O, no clock, no randomness, no locks, no JSON. CI checks this. The daemon passes in time, packets and random bytes. |
| `crates/wirebond` | `node` (event loop, control protocol, bonds, measure-and-select, DNS relay), `host` (per-OS sockets, TUN, routes, forwarding, NAT, DNS settings, service install, permissions), `update`, `cli`, `api` (local API types) | The only crate that touches the OS. |
| `crates/netsim` | the deterministic simulator and the acceptance suite | Runs `wirebond-core` on simulated links and replays recorded traces. |
| `crates/load` | the load and echo tool for the system harness | Replaces the Go tool. |
| `crates/view` | the GPUI viewer | Built and released separately. It depends on `api` for the status types. |

### 4.3 Event loop

The daemon runs one thread with one loop. It waits on the TUN device, the lane sockets, the control sockets and the local API, with a timeout taken from the engine's next deadline.

- **Linux and macOS:** `poll`.
- **Windows:** every socket is associated with one event object through `WSAEventSelect`. The loop waits with `WaitForMultipleObjects` on three handles: that event, Wintun's read-wait event and a high-resolution waitable timer. It then drains each source without blocking. The daemon also asks for 1 ms timers (`timeBeginPeriod(1)`) for as long as it runs.

Per packet, the cost is one TUN read and one `sendto` per lane it goes out on; on receipt, one `recvfrom` and one TUN write. Batching (`recvmmsg`/`sendmmsg`, UDP GSO/GRO) is deferred (§26).

## 5. Capture on the client

### 5.1 The interface

| | Value |
| --- | --- |
| Name | `wb0` on Linux, `utunN` on macOS, an adapter named `wirebond` on Windows |
| MTU | `TUN_MTU` (1400). A lane packet then fits 1500 bytes over IPv4 (1400 + 33 + 28 = 1461) and over IPv6 (1481). TCP over it negotiates an MSS of 1360. |
| Addresses | as given by the anchor in `bond ok`: 10.77.0.N/24 and fd77:b0d::N/64, or 192.168.137.N/24 from a Windows anchor |

### 5.2 What goes in, what stays out

All IPv4 and IPv6 traffic enters the interface, except:

1. **The mesh:** its address ranges, and the packets the mesh itself sends over the physical links.
2. **wirebond's own lane sockets:** each is pinned to its physical link.
3. **Each link's local subnets,** plus link-local, multicast and broadcast traffic. Printers, a hotel's gateway and AirPlay stay local.
4. **Loopback.**

DNS is captured and answered at the anchor (§7).

If the anchor has no IPv4 egress and no IPv6 egress, the client routes only the bond's subnets into the interface. Traffic for the anchor itself still uses the bond; everything else goes direct, and status shows `no_egress`.

### 5.3 Per OS

**Linux**
- **Device:** open `/dev/net/tun` with `IFF_TUN | IFF_NO_PI`.
- **Routes:** routing table `7849` holds `default dev wb0`, for IPv4 and IPv6.
- **Rules:**
  - priority 5299: `lookup main suppress_prefixlength 0`, so every route more specific than a default wins (local subnets);
  - priority 5300: `not fwmark 0x7849 lookup 7849`.
- **Coexisting with Tailscale:** Tailscale's own rules sit at priorities 5210–5270 and mark its underlay packets with fwmark 0x80000, so its traffic and its mesh routes are matched first.
- **Lane sockets:** `SO_MARK 0x7849` and `SO_BINDTODEVICE <link>`. Every link carrying a lane gets `rp_filter` 2 (loose).
- **Tools:** routes and rules are set with `ip` (iproute2). The daemon checks for it at start and reports `missing_tool ip` if it is absent.

**macOS**
- **Device:** a `utun` interface from a `PF_SYSTEM`/`SYSPROTO_CONTROL` socket connected to `com.apple.net.utun_control`. It needs root.
- **Routes:** `0.0.0.0/1`, `128.0.0.0/1`, `::/1` and `8000::/1` point to the utun. They cover the default without replacing it. Any more specific route keeps winning, including the local subnets and the mesh's 100.64.0.0/10 and fd7a:115c:a1e0::/48 on Tailscale's own utun.
- **Lane sockets:** pinned with `IP_BOUND_IF` / `IPV6_BOUND_IF`.

**Windows**
- **Device:** Wintun 0.14.1 (`wintun.dll`, signed by WireGuard LLC). It is embedded in `wirebond.exe`, written beside it at install and checked against its SHA-256 before every load.
- **Adapter:** named `wirebond`, interface metric 5.
- **Routes:** `0.0.0.0/1`, `128.0.0.0/1`, `::/1` and `8000::/1` through `CreateIpForwardEntry2`.
- **Lane sockets:** bound to the link's address, with `IP_UNICAST_IF` / `IPV6_UNICAST_IF`.

**The mesh's own packets on macOS and Windows.** tailscaled is expected to bind its underlay sockets to the physical default interface, as its own exit nodes require, so they bypass the `/1` routes. Tailscale's documentation does not state this, so the daemon checks it once the bond is up. If the mesh's traffic appears inside the interface, it removes capture and reports `mesh_captured`.

### 5.4 Installing and removing capture

- **Installed** when the bond has at least one validated lane.
- **Removed** on `client off`, on a role change, on fail-open (§17) and on daemon exit.
- **After a crash:** at start the daemon deletes any wirebond routes, rules and addresses a crash left behind before doing anything else.

### 5.5 IPv6 without IPv6 egress

When the anchor offers IPv4 egress only, the client still captures IPv6, so nothing leaks around the bond. It answers each outbound IPv6 packet for the internet with ICMPv6 destination unreachable (no route), at most 100 a second, so applications fall back to IPv4 at once.

## 6. Egress on the anchor

### 6.1 Interface and local delivery

The anchor's interface has 10.77.0.1/24 and fd77:b0d::1/64, or 192.168.137.1/24 on Windows (§6.2).

- **Delivered locally:** packets for these addresses go to the anchor's own services. Moonlight on the client adds the host at the anchor's bond address (`10.77.0.1`, or `192.168.137.1` for a Windows anchor), and status prints that line.
- **Not routed through the bond:** the anchor's mesh address. Traffic to it, the control channel included, stays on the mesh.

### 6.2 Forwarding and NAT

| OS | Forwarding | NAT | IPv6 egress |
| --- | --- | --- | --- |
| Linux | `net.ipv4.ip_forward=1`, `net.ipv6.conf.all.forwarding=1`, saved and restored on exit | an nftables table `inet wirebond`: forward chain accepts `wb0` → uplink for the bond's sources and established/related traffic back; postrouting `masquerade` for the bond's sources leaving any other interface | yes (masquerade for IPv6) |
| macOS | `net.inet.ip.forwarding=1`, `net.inet6.ip6.forwarding=1` | pf rules loaded into the anchor `com.apple/wirebond`, which the default `/etc/pf.conf` already includes through `nat-anchor "com.apple/*"`: `nat on <uplink> inet from 10.77.0.0/24 -> (<uplink>)` and the IPv6 equivalent; pf enabled with `pfctl -E`, and that reference released on exit | yes |
| Windows | Internet Connection Sharing (ICS), through the `HNetCfg.HNetShare` COM object, shares the uplink to the `wirebond` adapter and readdresses it to 192.168.137.1/24 | ICS's own NAT. It works on Home and Pro; WinNAT's `New-NetNat` needs Hyper-V, which Home lacks | no |

On Windows, ICS serves one shared adapter at a time. If it is already in use (Mobile Hotspot also uses it) or is refused, the anchor still delivers locally and offers no egress (§5.2). On Linux the daemon reports `missing_tool nft` if nftables is absent. The uplink is the interface of the anchor's default route; it is re-checked every `LINK_SCAN` (2 s) and the NAT rules follow it.

### 6.3 Safety

- The anchor forwards a packet from its interface only if the source is an address it assigned to a live bond. It drops everything else and counts it.
- Clients cannot reach each other. Forwarding from one bond address to another is dropped.
- Nothing from the internet enters a bond except replies to connections the client opened (NAT state). There are no inbound port forwards.

## 7. DNS

**The anchor's relay**
- It listens on UDP and TCP port 53 of the anchor's bond addresses.
- It forwards each query verbatim to the anchor's own first resolver:
  - Linux: the first `nameserver` in `/etc/resolv.conf`;
  - macOS: resolver #1 from `scutil --dns`;
  - Windows: the first DNS server of the default-route adapter, from `GetAdaptersAddresses`.
- It relays each answer back to the client that asked.
- Limits: at most `DNS_INFLIGHT` (256) queries per bond; a query with no answer after `DNS_TIMEOUT` (5 s) is dropped. It caches nothing and reads only the 2-byte query ID.

**The client** points its resolver at the anchor's bond address while bonded:

| OS | How |
| --- | --- |
| Linux | systemd-resolved: `resolvectl dns wb0 10.77.0.1`, `resolvectl domain wb0 '~.'`, `resolvectl default-route wb0 yes`. Without systemd-resolved, DNS is left alone and status shows `dns_local`. |
| macOS | A dynamic-store service for the utun, with `ServerAddresses` and an empty `SupplementalMatchDomains`, written through `scutil`. |
| Windows | `SetInterfaceDnsSettings` on the adapter (Windows 10 2004 or later), with interface metric 5. |

Every change is undone when capture is removed. Resolving near the anchor means CDNs pick servers near where the traffic actually leaves.

## 8. Supports

**A phone on USB.** Nothing runs on the phone; the client sees a new link.

| Phone | macOS | Windows | Linux |
| --- | --- | --- | --- |
| iPhone (Personal Hotspot over USB) | `iPhone USB` | Apple Mobile Device Ethernet | `ipheth` |
| Android (USB tethering) | — | RNDIS or NCM interface | RNDIS or NCM interface |

**A computer as a support.** `wirebond support on <uplink> <cable> [--for <client>]` shares `<uplink>` to `<cable>` with the OS's own sharing:

| OS | How |
| --- | --- |
| Linux | NetworkManager: `nmcli connection modify <cable> ipv4.method shared`. It brings DHCP and NAT. |
| macOS | Internet Sharing, configured through `com.apple.nat` and started through its launchd job. |
| Windows | Internet Connection Sharing, through the `HNetCfg.HNetShare` COM object driven by PowerShell. |

If the OS refuses, the command prints the manual steps. With `--for`, the support registers with that client over the mesh, telling it the cable's subnet. The client then names that link after the support, and the support can show its contribution (§20).

**Bottleneck groups.** Each link is its own group, unless two links show the same public IPv4 from STUN or the same default gateway; then they share one. The mesh lane belongs to the group of the link that carries the mesh.

## 9. Lanes and discovery

### 9.1 Links and sockets

- Each node lists its links every `LINK_SCAN` (2 s).
- Each node opens one UDP socket per link, pinned to it (§5.3), on port 7849, or on an ephemeral port if 7849 is taken. Buffers are `SOCK_BUF` (4 MiB) each way.
- Link ids run 1–8 per side (`MAX_LINKS`).
- A lane id is `client link << 8 | anchor link`. Lane 0 is the mesh lane, between the two mesh addresses.

### 9.2 STUN

- Each link learns its public mapping with an RFC 8489 Binding request to the configured STUN servers (defaults: `stun.l.google.com:19302`, `stun.cloudflare.com:3478`).
- It refreshes every `STUN_EVERY` (5 min) while data flows, and on every link change.
- A reply is accepted only from a server queried within `STUN_WAIT` (3 s).

### 9.3 Candidates

A link offers at most `MAX_CANDS` (4) candidates: its local addresses and its STUN mappings. They travel in `links` (§16).

### 9.4 Punching and validation

- **Punching:** both sides probe every candidate pair every `PROBE_PUNCH` (200 ms) for `PUNCH_FOR` (5 s) after any link or candidate change, a wake, or a new control channel. At most one round per peer per `PUNCH_GAP` (5 s).
- **Validation:** a lane carries data only once validated. An echo must return the challenge of a probe sent to that address (§11.2). Until then, an address gets at most `VALIDATE_AMP` (3×) the bytes received from it.

## 10. Security

### 10.1 Control channel

- It runs over the mesh, which authenticates and encrypts it. The daemon listens on its mesh addresses only.
- An anchor accepts `bond open` only from allow-listed devices, identified by mesh address.

### 10.2 Keys

- **Send key:** each side holds one per bond: 32 random bytes from the OS. It travels in `key` over the control channel and is never written to disk.
- **rid:** a 2-byte key id that selects the key.
- **Ratchet:** every 2^24 packets (`EPOCH_BITS` 24), K_e = HMAC-SHA256(K_{e−1}, "wirebond epoch").
- **Receiver:** holds the previous, current and next epoch's keys, and derives the next at most once.
- **Erasure:** dropped keys, AES round keys and HMAC state are zeroed (`zeroize`).
- **Libraries:** RustCrypto `aes-gcm` 0.11, `hmac` 0.13 and `sha2` 0.11. They are pure Rust and use AES instructions where the CPU has them.

### 10.3 Packet

Integers are big-endian.

```
 0      1        3                     11                       end
 +------+--------+---------------------+---------------------+----------+
 | type |  rid   |   counter (u64)     |   sealed payload    | tag (16) |
 +------+--------+---------------------+---------------------+----------+
 |<---- additional data (11 bytes) --->|
```

- **`type`:** `0x01` data, `0x02` probe. Bit `0x80` on data marks a small-flow packet, which is delivered without reordering (§15).
- **`counter`:** one per send key. It covers every packet type and every lane, and never repeats. Its top 40 bits name the epoch.
- **Nonce:** 12 bytes: two zero bytes, then `rid`, then `counter`.
- **Sealing:** AES-256-GCM, with the 11-byte header as additional data.
- **Sealed once:** a packet is sealed when first queued. Every copy on every lane is the same bytes, and the replay window keeps the first to arrive.

**Data payload**

| Field | Bytes | Meaning |
| --- | --- | --- |
| `flow` | 2 | The sender's flow number for this bond and direction, from 1. |
| `seq` | 4 | Per flow and direction, from 1, compared in serial arithmetic (RFC 1982). |
| packet | rest | One inner IPv4 or IPv6 packet. |

Overhead is 11 + 6 + 16 = **33 bytes**, plus 28 (IPv4/UDP) or 48 (IPv6/UDP) for the lane.

### 10.4 Receiving, in order

1. Look up `rid`. Unless the epoch is the key's previous, current or next and the counter is not below the key's floor, drop the packet: no reply, no state.
2. Open it. On failure, drop: no reply, no state.
   - **A later copy is not opened.** Before opening a data packet, read the bond's replay window (step 4) without changing it. If it already holds the counter, the packet is a later copy of one opened before, or a replay: it is counted at step 3 for its lane, if its type and length are that opened packet's, and stops there; otherwise it is dropped with no state. In performance mode every packet arrives once per lane of the spread, and opening each copy only for step 4 to drop it doubled the receive side's AES-GCM work.
   - Counted for its lane means the lane's replay window, the `pkts`, `bytes` and `last_counter` its echo reports (§11.2), its lagging mark (§11.4), its contribution statistics (§11.5) and, for bulk, which `seq` of its flow the lane brought (§15), known from the opened packet. Nothing in it is read, delivered, forwarded or answered, and every copy is the same bytes, so an unopened copy records exactly what a replay of the real copy on that lane would; the bond's window and keys change only after step 2 succeeds (§10.5 rule 1).
3. Check the lane's replay window (`REPLAY_WINDOW` 8,192 counters, per lane and key). A repeat on the same lane stops here.
4. Check the bond's replay window (8,192 counters per key). A later copy stops here, after it has been counted for the lane's statistics.
5. Act on the payload.

### 10.5 Rules

1. Nothing that fails steps 1–2 changes state or gets a reply.
2. A lane's return address moves only on an authenticated probe, for that lane, with a higher `seq`, arriving on the lane's own socket. After a move, the lane is unvalidated until a challenge returns (§9.4).
3. No reply is ever larger than the packet that caused it.
4. Every buffer is bounded. A bond's pacer queues, reorder buffers and rewind rings together use at most `BOND_BYTES` (16 MiB).
5. Nothing depends on the wall clock.
6. Inner packets are checked:
   - the client delivers only packets addressed to its own bond address;
   - the anchor forwards only packets from the client's bond address (§6.3).
7. A `hello` with a new `inst` from a mesh address ends that address's older bond at once.

## 11. Measurement

### 11.1 Cadence

| When | Probe |
| --- | --- |
| Data flowed within `IDLE_AFTER` (5 s) | every lane, every `PROBE_ACTIVE` (20 ms) |
| Punching | every candidate pair, every 200 ms, for 5 s |
| Idle | one probe every `PROBE_IDLE` (25 s) on one direct lane; down after `IDLE_MISSES` (3) misses |

### 11.2 Probe and echo

**Probe:** the sealed payload of a `0x02` packet.

| Field | Bytes | Meaning |
| --- | --- | --- |
| lane | 2 | lane id |
| seq | 4 | per lane and sender, never 0, serial order |
| sent_at | 8 | sender's monotonic clock, µs |
| flags | 1 | bit 0: this lane is in my copy set; bit 1: answer at once (validation); bit 2 (`PROBE_REWIND`): a rewind of mine is in flight (§14.2, §15) |
| n | 1 | number of echoes |
| echoes | 53 × n | one per lane received on in the last `ECHO_WINDOW` (2 s) |

**Echo**

| Field | Bytes | Meaning |
| --- | --- | --- |
| lane | 2 | lane id |
| flags | 1 | bit 0: the lane is lagging |
| seq | 4 | the newest probe received on this lane |
| held | 4 | µs from that probe's arrival to this probe's sending |
| owd | 8 | µs, arrival time minus that probe's `sent_at`; only differences are used |
| pkts | 4 | packets received on this lane, copies included, when that probe arrived (wraps) |
| bytes | 4 | bytes in the same snapshot (wraps) |
| last_counter | 8 | the counter of the newest data packet received on this lane |
| token | 8 | the challenge that probe carried, returned; 0 if none |
| chal | 8 | in the echo of this probe's own lane: this probe's challenge, or 0 |
| pad | 2 | zero |

- **Challenge:** the first 8 bytes of HMAC-SHA256, under a random per-bond key, over the lane id, an address number and the probe's `seq`. A result of 0 becomes 1.
- **Size:** at most `MAX_ECHOES` echoes fit in one packet. With three lanes, a probe costs about 92 kbit/s per lane per direction while active.

### 11.3 Estimates per lane

| Estimate | How |
| --- | --- |
| RTT | EWMA with gain 1/8 |
| one-way delay (OWD) | from probe pairs; base OWD is the minimum over `MIN_WINDOW` (10 s) |
| jitter (`dev`) | median absolute deviation of OWD over `DEV_WINDOW` (2 s), from up to `MAD_SAMPLES` (128) samples |
| loss, delivery rate | over `RATE_WINDOW` (100 ms) |

### 11.4 Liveness

| State | Condition |
| --- | --- |
| suspect | no echo within RTT + max(`SUSPECT_MIN` 20 ms, `SUSPECT_DEV` 4 × RTT jitter) |
| lagging | a copy arrives on this lane later than max(`LAG_FLOOR` 10 ms, `LAG_JITTER` 3 × dev) plus the lanes' base-OWD difference |
| down | no echo for `DOWN_AFTER` (2 s) while active, blind or not |
| up again | `UP_AFTER` (1 s) of clean echoes |
| blind | no echo on any lane for `BLIND_AFTER` (500 ms): bulk goes on the two lowest-OWD validated lanes in different groups, up ones first, then suspect or down ones alike, direct lanes before the mesh lane, until echoes return. A lane that goes down while blind stays in the pair and keeps its rate; a lane back up restarts its rate only if it carried no bulk within `MIN_WINDOW`. |
| wake | sleep-inclusive time jumps more than `WAKE_GAP` (1 s) beyond the monotonic clock, or the monotonic clock jumps more than `WAKE_JUMP` (3 s) between ticks. The node re-punches and discards OWD samples from before the wake. |

### 11.5 Contribution statistics

These are kept per lane, per minute and per bond.

| Statistic | Meaning |
| --- | --- |
| bytes carried | each direction |
| share | this lane's fraction of the bond's bytes |
| first arrivals | performance mode: the fraction of packets whose first copy came on this lane |
| rescues | performance mode: packets whose first copy came on this lane while every other copy came at least `RESCUE_GAP` (10 ms) later or never. Kept in a ring of 8,192 counters. |
| reorder wait | bandwidth mode: p50 and p99 |

## 12. Rate control and pacing

Each lane has its own delay-based controller and token-bucket pacer.

| Part | Rule |
| --- | --- |
| Target queueing delay | max(`TARGET_FLOOR` 20 ms, `TARGET_DEV` 2 × dev, while app-limited) |
| Overuse | loss ≥ `LOSS_OVERUSE` (2%) and ≥ `LOSS_MIN` (5) packets within `RATE_WINDOW`, or queueing above target |
| On overuse | cut the rate, never below `BETA` (0.85) × the current rate |
| Startup | × `STARTUP_GAIN` (1.25) per RTT while backlogged, until the first overuse |
| Then | + `INCREASE_STEP` (1%) of rate per RTT |
| Floor | `PACE_FLOOR` (5 Mbit/s) |
| Bucket depth | `PACE_BURST` (1 ms) at the pace, plus one packet |
| Queue age | a packet older than `QUEUE_AGE` (100 ms) in a pacer queue is dropped |
| Largest lane packet | `MAX_PACKET` (1500 bytes) |

## 13. Flows and classes

**Key**

| Traffic | Key |
| --- | --- |
| TCP, UDP | protocol, source and destination address, source and destination port |
| ICMP, ICMPv6 | protocol, addresses, echo id |
| Other protocols, and every fragment of a datagram, the first included (IPv4: MF set or a non-zero offset; IPv6: a Fragment header) | protocol and addresses |

Every fragment takes the port-less key so that a datagram's fragments stay in one flow, in order. Moonlight's default 1,392-byte payloads exceed the 1,372 bytes a 1,400-byte TUN carries whole (1,400 − 20 − 8), so its video crosses the bond fragmented; keyed apart, a datagram's first fragment went with the video's bulk flow through the reorder buffer and its last one into a small flow copied at once, and the receiving kernel failed to reassemble them (`Ip.ReasmFails`).

**Numbering**
- The sender numbers flows per bond and direction: a u16 from 1, reused after `FLOW_IDLE` (60 s) without traffic.
- `seq` counts per flow and direction, from 1.

**Classes**
- A flow direction under `SMALL_RATE` (1 Mbit/s) over `SMALL_WINDOW` (200 ms) is **small**. It is copied on every lane in performance mode, and on two lanes in different groups in bandwidth mode, within `SMALL_COPY_BUDGET` (2 Mbit/s) per bond.
- Any other flow is **bulk**.

**Limits**
- At most `MAX_FLOWS` (4096) live flows per bond. A packet that would open another evicts the flow idle longest.
- At most `REORDER_SLOTS` (2048) held packets per flow.
- All of this counts within `BOND_BYTES`.

## 14. Modes

The mode belongs to the bond. The client sets it, and it applies in both directions. A change applies to the next packet; buffers drain under the old rules.

### 14.1 Performance (default)

- **The spread:** one lane per bottleneck group that is up, validated, not suspect and not barred. In each group, the lane with the lowest base OWD. The mesh lane is in the spread only for a group with no usable direct lane: a direct lane is usable while it is validated and up or suspect, barred or not. While blind, and while no lane qualifies though echoes still arrive, the spread is the blind pair (§11.4).
- **Sending:** every packet of every flow goes on every lane of the spread.
  - Each lane's controller is offered the whole stream and drops its excess at its own pacer, so a lane too slow for the stream never holds the others back.
  - A lane that leaves the spread drops its queue.
  - Nothing ever switches, re-sends or tests a lane.
- **Receiving:** the first copy wins (§10.4).
  - When a gap opens in a flow and some lane has already delivered a higher `seq`, the receiver waits for the missing packet only max(0, slower lane's base OWD − passing lane's base OWD) + `DUP_GRACE` (2 ms).
  - It then releases what it holds; the missing packet counts as lost, and if it arrives later it is delivered at once (§15).
  - A known limitation: the wait counts base OWDs only, so a copy still queued in the sender's pacer on the lane with the lower base OWD can arrive after its gap was given up, and is then delivered out of order.
- **Cost:** each lane carries the full stream: about 13.5 GB per hour per lane at 30 Mbit/s.

### 14.2 Bandwidth

**Eligible lanes** are:
- up, validated, not suspect and not barred;
- at most one per group: the lowest base OWD;
- within `BANDWIDTH_SKEW_MAX` (50 ms) of the fastest eligible lane's base OWD (the fastest lane is the one with the lowest base OWD);
- carrying at least a tenth of the fastest lane's rate.

The mesh lane is eligible only for a group with no usable direct lane (validated, and up or suspect, barred or not). An empty eligible set falls back to the blind pair (§11.4). A lane that is the whole spread starts and climbs as a performance lane does (unlimited in startup), but only while no other direct lane is suspect yet still echoed within its RTT; once another lane shares the stream, it paces from its delivery rate. On a change to bandwidth, a lane still unlimited from performance starts again from `PACE_FLOOR` (§12).

**Earliest arrival.** Each bulk packet joins the pacer of the lane with the smallest estimated arrival: the lane's newest echoed OWD + 2 × dev + (queued bytes + the packet's size) ÷ pace. The newest OWD, not the base OWD, since it carries the standing queue of the lane's path, which the base OWD does not see; by the base OWD alone, bulk over paths with 60 ms buffers arrived later than `REORDER_MAX` behind its gaps. A lane other than the fastest must win by `ECF_MARGIN` (1 ms).

**Small flows** are copied (§13).

**Leaving the spread.** A lane in overuse at `PACE_FLOOR` for `BANDWIDTH_FLOOR_HOLD` (500 ms) is barred for `BAR` (60 s). The bar doubles, up to `BAR_MAX` (10 min), while the lane keeps failing.

**Rewind.** When a lane turns suspect, its unacknowledged packets from the last `REWIND_WINDOW` (300 ms, or its suspect timeout plus RTT if longer) are re-sent on the other eligible lanes. A packet's echoed `last_counter` marks it acknowledged.

## 15. Reorder and delivery

- **Small-flow packets** (type bit `0x80`) are written to the interface as soon as they pass §10.4.
- **Bulk packets are released in `seq` order per flow:**
  - in performance mode, under §14.1's rule;
  - in bandwidth mode, a gap waits until every lane that carried the flow within `REORDER_LANES` (100 ms) has delivered a later `seq`, or until a timer runs out: the larger of the lanes' base-OWD skew and the flow's learned fill (how long its gaps took to fill, decaying by 1/`SKEW_DECAY` at each filled gap, at most what `REORDER_MAX` allows) × `SKEW_MARGIN` (1.25), capped at `REORDER_MAX` (50 ms), or `REORDER_RESCUE` (150 ms) while a carrying lane lags or a rewind is in flight (the peer's probe flag `PROBE_REWIND`, §11.2).
- **A bulk packet that arrives after its gap was given up** is delivered at once, never dropped: it is out of order, but whole.
- **Order across flows** is never kept.

## 16. Control protocol

**Framing**
- One JSON object per line, UTF-8, at most `MAX_MSG` (64 KiB).
- A sender queues at most `CTL_QUEUE` (64) messages per channel, then closes it.
- TCP keepalive: idle `KEEP_IDLE` (5 s), interval `KEEP_INTERVAL` (2 s), `KEEP_COUNT` (3). A vanished peer is found in about 11 s.
- Unknown fields are ignored. Unknown message types are answered with `error` `unknown`.

| Message | Fields | Sent |
| --- | --- | --- |
| `hello` | `v` (2), `inst` (u64, random per process), `name`, `role`, `version` (semver) | first, by both sides |
| `error` | `code`, `msg` | on any refusal |
| `bond` | `op`: `open` (client: `mode`), `ok` (anchor: `addr4`, `addr6`, `mtu`, `dns`, `egress` = `v4`/`v6`/`both`/`none`), `close` | lifecycle (§3.2) |
| `key` | `rid`, `key` (base64, 32 bytes), `epoch` | at bond open and on each new epoch |
| `links` | `links`: [{`id`, `kind` (`wifi`, `ethernet`, `usb`, `cellular`, `other`), `metered`, `group`, `support` (support device name or null), `mesh` (true on the link carrying the mesh's underlay: the mesh lane joins its group, §8), `cands`: ["ip:port", …]}] | at bond open, and on each link or candidate change |
| `mode` | `mode`: `performance` or `bandwidth` | client, on change |
| `support` | `cable_subnet`, `name` | support → client, after `wirebond support on … --for` |
| `measure` | `op`: `start` (`seconds`, `bulk_bytes`), `result` (per lane: `rtt_p50`, `owd_p99`, `dev`, `loss`, `mbps`; `egress_ms`) | during `pick` (§18) |
| `view` | request: {}; reply: the status JSON of §20.2, without flow destinations | from any mesh peer |

Error codes: `version`, `not_allowed`, `too_many_bonds`, `busy` (a `pick` in progress), `unknown`, `bad_request`.

## 17. Failure handling

| Event | Behaviour |
| --- | --- |
| Anchor unreachable at start | No capture; traffic goes direct. Problem `anchor_unreachable`. Redial with backoff from `REDIAL_MIN` (1 s) to `REDIAL_MAX` (30 s). |
| Bond lost | Every lane down and the channel down for `FAIL_OPEN_AFTER` (5 s): capture is removed and traffic goes direct (fail open). Problem `bond_down`. Capture returns when a lane validates again; direct connections opened meanwhile break then. |
| Captive portal | On link up, the node fetches `http://captive.apple.com/hotspot-detect.html` over that link. Anything but `Success` marks the link captive, and it carries no lane. If the bond is up through another link, the login page would go through the anchor, so status shows `captive_portal <link>`: run `wirebond client off`, log in, then `wirebond client <anchor>`. |
| Sleep and wake | Wake detection (§11.4): re-punch, fresh estimates, keys kept. |
| A link appears or vanishes | Rescan within 2 s, re-punch, recompute groups. |
| Mesh down, lanes up | Direct lanes keep carrying the bond. Keys ratchet by derivation. The channel redials. |
| Every lane down, channel up for `ALL_LANES_DOWN` (2 s) | Redial the channel, then re-punch. |
| Only the mesh lane after `MESH_ONLY_AFTER` (30 s) | Problem `mesh_only`: the bond gives no gain over the baseline. |
| No lane validated `LANES_BLOCKED_AFTER` (3 s) after connect | Problem `lanes_blocked`. |

## 18. Measure and select: `wirebond pick`

`wirebond pick [<device>…] [--mode performance|bandwidth] [--seconds N] [--dry-run]`

**Candidates** are the named devices, or else `config.anchors`, a list kept with `wirebond pick --add <device>` and `--remove <device>`. A candidate must answer `hello` with role `anchor` and accept this client.

**Measure phase.** Candidates are measured one after another, each for `PICK_SECONDS` (10 s, or `--seconds`):
1. Open a temporary bond with no capture, and bring its lanes up (§9).
2. For half the time, probe every lane every 20 ms: RTT p50, OWD p99, dev and loss per lane, and the p99 of the per-probe minimum OWD across lanes in different groups. That last figure is what performance mode would deliver.
3. Bandwidth mode only: for the other half, push paced filler on every lane in parallel, ramping until overuse, at most `PICK_BULK_BYTES` (25 MB) per candidate. Each lane's top delivery rate is recorded.
4. The candidate measures its own egress: the median TCP connect time over `PICK_EGRESS_TRIES` (5) attempts to each of `PICK_REFERENCES` (1.1.1.1:443, 8.8.8.8:443, 9.9.9.9:443), reported as `egress_ms`.

**Selection phase**

| Mode | Score | Best |
| --- | --- | --- |
| performance | p99 of the across-lane minimum OWD + `egress_ms` ÷ 2 | lowest |
| bandwidth | sum of the eligible lanes' top rates | highest; ties within 5% go to the lower OWD p50 |

- A candidate with no validated lane, or with every lane in one group, scores last and is printed with the reason.
- `pick` prints one row per candidate (lanes, groups, score), then the winner.
- Without `--dry-run`, it sets the winner as the anchor and reconnects the bond, which breaks open internet connections.

**Cost:** about 10 s and 0.3 MB per candidate in performance mode; up to 25 MB per candidate in bandwidth mode. A later version may run `pick` on its own (§26).

## 19. CLI

```
wirebond                                   same as: wirebond status
wirebond status [<device>] [--json]        this node, or another node's read-only view over the mesh
wirebond client <anchor> [performance|bandwidth]
                                           make this node a client of <anchor> and start its bond
wirebond client off                        stop the bond; traffic goes direct
wirebond mode performance|bandwidth        change the bond's mode
wirebond pick [<device>...] [--mode M] [--seconds N] [--dry-run]
wirebond pick --add <device> | --remove <device>
wirebond anchor on|off                     accept bonds on this node
wirebond anchor allow <device>... | deny <device>...
wirebond support on <uplink> <cable> [--for <client>]
wirebond support off
wirebond update                            update now
wirebond uninstall                         remove everything wirebond created
wirebond version [--json]
wirebond run                               the daemon (started by the service manager)
```

- **Output:** human-readable by default; `--json` gives the §20.2 schema.
- **Installation:** the first command that changes anything installs and starts the service, which needs administrator rights once.

**Exit codes**

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 1 | the operation failed (a reason is printed) |
| 2 | usage |
| 3 | no daemon, or not installed |
| 4 | needs administrator rights |
| 5 | refused by the peer: `not_allowed`, `too_many_bonds`, `version` |

## 20. Status, local API and viewer

### 20.1 Local API

**Where it listens**

| OS | Endpoint | Access |
| --- | --- | --- |
| Unix | `/var/run/wirebond.sock` | owner root, group `wirebond`, mode 0660. Install adds the installing user to the group. |
| Windows | `\\.\pipe\wirebond` | Administrators: read and write; the interactive user: read |

**Protocol:** one JSON request and one JSON reply per line, at most 64 KiB. The verbs are those of the CLI.

### 20.2 Status JSON

```
{
  "device": "mac", "role": "client", "version": "2.0.0",
  "links": [{"id": 1, "name": "Wi-Fi", "kind": "wifi", "group": 1, "metered": false,
             "up": true, "captive": false, "support": null}],
  "bond": {"anchor": "xanth", "mode": "performance", "state": "up",
           "addr4": "10.77.0.2", "anchor_addr4": "10.77.0.1", "egress": "both",
           "lanes": [{"id": 257, "links": ["Wi-Fi", "eth"], "group": 1, "state": "up",
                      "rtt_ms": 61.0, "owd_ms": 30.2, "dev_ms": 1.1, "loss": 0.0,
                      "rate_mbps": 38.0, "tx_mbps": 30.1, "rx_mbps": 2.0, "share": 0.5,
                      "first_arrivals": 0.71, "rescues_per_min": 12, "bytes_today": 0}],
           "flows": 143, "small_flows": 9},
  "anchor": null,
  "support": null,
  "contribution": {"tx_bytes": 0, "rx_bytes": 0, "since": "start"},
  "mesh": [{"device": "xanth", "role": "anchor", "bonds": 1, "reachable": true}],
  "problems": [{"code": "captive_portal", "link": "Wi-Fi", "next": "wirebond client off"}],
  "events": ["…"]
}
```

What each role sees:
- **Anchor:** an `anchor` object listing its bonds, each client's addresses, bytes egressed and delivered locally, and its uplink.
- **Support:** its cable, its uplink and the bytes it carried.
- **Remote views** (`view`, or `wirebond status <device>`) omit flow details and public addresses.

### 20.3 Contribution

| Role | Contribution |
| --- | --- |
| Client | bytes on each of its links, with each link's share of the bond |
| Support | bytes it carried on its cable, from the OS counters, plus the share and rescues of the lane through it, from its client's view |
| Anchor | bytes it egressed and delivered locally, per bond |

### 20.4 Viewer: `wirebond-view`

**Framework: GPUI**, the Rust UI framework of the Zed editor. It runs on macOS, Linux and Windows, the platforms that run wirebond, and shares the status types with the daemon.

**Lynx** (ByteDance) renders native views from a JavaScript codebase on iOS, Android and the web, and on macOS and Windows since Lynx 3.7 (April 2026). It does not support Linux. It is deferred until a phone viewer is wanted (§26); it could then serve phones and desktops alike from the same `view` JSON over the mesh.

**Screens**

| Screen | Shows |
| --- | --- |
| This device | role, links, contribution |
| Bond | each lane's rate, share, delay, jitter, loss, first arrivals and rescues, with 60-second graphs |
| Mesh | nodes, their roles, bonds and health |

The viewer refreshes once a second. It is read-only and optional, and ships as separate release assets so the daemon stays small.

## 21. Limits and constants

Durations are monotonic. All constants live in `crates/core/src/consts.rs`.

| Constant | Value | Reason |
| --- | --- | --- |
| `PORT` | 7849 | control (TCP, on mesh addresses) and lanes (UDP) |
| `TUN_MTU` | 1400 | a lane packet fits 1500 bytes over IPv6 |
| `HEADER_LEN`, `TAG_LEN`, data fields | 11, 16, 6 | 33 bytes of overhead |
| `MAX_PACKET` | 1500 | largest lane packet |
| `MAX_MSG`, `CTL_QUEUE` | 64 KiB, 64 | control channel bounds |
| `SOCK_BUF` | 4 MiB | per UDP socket |
| `BOND_BYTES` | 16 MiB | all queues and buffers of one bond |
| `MAX_BONDS` | 8 | per anchor |
| `MAX_LINKS`, `MAX_CANDS` | 8, 4 | per node, per link |
| `MAX_FLOWS`, `REORDER_SLOTS`, `FLOW_IDLE` | 4096, 2048, 60 s | per bond, per flow |
| `PROBE_ACTIVE`, `PROBE_PUNCH`, `PUNCH_FOR`, `PUNCH_GAP` | 20 ms, 200 ms, 5 s, 5 s | §9, §11 |
| `PROBE_IDLE`, `IDLE_AFTER`, `IDLE_MISSES` | 25 s, 5 s, 3 | idle cost: about 70 KB per bond per hour |
| `ECHO_WINDOW`, `RTT_RING`, `ECHO_HISTORY`, `MAD_SAMPLES` | 2 s, 256, 8, 128 | §11 |
| `EWMA_GAIN`, `DEV_WINDOW`, `MIN_WINDOW`, `RATE_WINDOW` | 1/8, 2 s, 10 s, 100 ms | §11.3 |
| `SUSPECT_MIN`, `SUSPECT_DEV`, `LAG_FLOOR`, `LAG_JITTER` | 20 ms, 4, 10 ms, 3 | §11.4 |
| `DOWN_AFTER`, `UP_AFTER`, `BLIND_AFTER` | 2 s, 1 s, 500 ms | §11.4 |
| `WAKE_GAP`, `WAKE_JUMP` | 1 s, 3 s | §11.4 |
| `TARGET_FLOOR`, `TARGET_DEV`, `LOSS_OVERUSE`, `LOSS_MIN` | 20 ms, 2, 2%, 5 | §12 |
| `BETA`, `STARTUP_GAIN`, `INCREASE_STEP` | 0.85, 1.25, 1% | §12 |
| `PACE_FLOOR`, `PACE_BURST`, `QUEUE_AGE` | 5 Mbit/s, 1 ms, 100 ms | §12 |
| `SMALL_RATE`, `SMALL_WINDOW`, `SMALL_COPY_BUDGET` | 1 Mbit/s, 200 ms, 2 Mbit/s | §13 |
| `DUP_GRACE` | 2 ms | performance receiver |
| `RESCUE_GAP` | 10 ms | a copy this much earlier is a rescue |
| `BANDWIDTH_SKEW_MAX`, `ECF_MARGIN`, `BANDWIDTH_FLOOR_HOLD` | 50 ms, 1 ms, 500 ms | §14.2 |
| `BAR`, `BAR_MAX` | 60 s, 10 min | §14.2 |
| `REWIND_WINDOW`, `REWIND_SHARE` | 300 ms, 0.5 | §14.2 |
| `REORDER_MAX`, `REORDER_RESCUE`, `REORDER_LANES`, `SKEW_MARGIN`, `SKEW_DECAY`, `REORDER_SHARE` | 50 ms, 150 ms, 100 ms, 1.25, 256, 0.75 | §15 |
| `REPLAY_WINDOW`, `EPOCH_BITS`, `VALIDATE_AMP` | 8192, 24, 3 | §10 |
| `LINK_SCAN`, `STUN_EVERY`, `STUN_WAIT` | 2 s, 5 min, 3 s | §9 |
| `KEEP_IDLE`, `KEEP_INTERVAL`, `KEEP_COUNT` | 5 s, 2 s, 3 | §16 |
| `REDIAL_MIN`, `REDIAL_MAX`, `ALL_LANES_DOWN`, `FAIL_OPEN_AFTER` | 1 s, 30 s, 2 s, 5 s | §17 |
| `LANES_BLOCKED_AFTER`, `MESH_ONLY_AFTER` | 3 s, 30 s | §17 |
| `DNS_INFLIGHT`, `DNS_TIMEOUT` | 256, 5 s | §7 |
| `PICK_SECONDS`, `PICK_BULK_BYTES`, `PICK_EGRESS_TRIES` | 10 s, 25 MB, 5 | §18 |
| `STATUS_EVENTS`, `LOG_LINES` | 20, 500 | §20 |
| `UPDATE_FIRST`, `UPDATE_EVERY`, `UPDATE_JITTER` | 5 min, 24 h, 60 min | §22.3 |
| `UPDATE_IDLE`, `UPDATE_HEALTHY`, `UPDATE_MAX_STARTS`, `SMOKE_TIMEOUT` | 10 min, 60 s, 3, 10 s | §22.3 |

## 22. Install, update and platforms

### 22.1 Platforms and assets

| OS | Daemon assets | Roles |
| --- | --- | --- |
| Linux (x86_64, aarch64, ARMv6 also published as armv7l) | static musl: `wirebond-Linux-x86_64`, `-aarch64`, `-armv7l`, `-armv6l` | client, anchor (IPv4 and IPv6 egress), support |
| macOS (arm64, x86_64) | `wirebond-Darwin-arm64`, `-x86_64` | client, anchor (IPv4 and IPv6 egress), support |
| Windows 10 2004+ (x64, ARM64) | `wirebond-Windows-AMD64.exe`, `-ARM64.exe`, with Wintun embedded | client, anchor (IPv4 egress through ICS), support |

- **Viewer assets:** `wirebond-view-Darwin-arm64`, `-Darwin-x86_64`, `-Linux-x86_64` and `-Windows-AMD64.exe`.
- **Every release** carries `SHA256SUMS`.

### 22.2 Service

Every role runs as a system service:

| OS | Service | Binary |
| --- | --- | --- |
| Linux | systemd unit `wirebond.service` | `/usr/local/bin/wirebond` |
| macOS | LaunchDaemon `com.wirebond` | `/Library/Application Support/wirebond/wirebond`: root-owned, because Homebrew can make `/usr/local/bin` writable by the user, and a root service must never run a file its user can replace |
| Windows | service `wirebond`, plus a firewall rule for its executable | `%ProgramFiles%\wirebond\wirebond.exe` |

- **Requirements checked at install:** Linux needs `ip` and `nft`, and `nmcli` for `support`. macOS needs nothing extra.
- **`wirebond uninstall`** removes the service, binary, config, routes, rules, NAT, DNS settings and the Wintun adapter.

### 22.3 Update

- **Schedule:** a check `UPDATE_FIRST` (5 min) after start, then every `UPDATE_EVERY` (24 h) ± `UPDATE_JITTER` (60 min), or on `wirebond update`.
- **Download and verify:** the system `curl` downloads the asset and `SHA256SUMS`, and the hash must match.
- **Smoke test:** `<exe>.new version --json` must answer within `SMOKE_TIMEOUT` (10 s).
- **Swap:** the new binary replaces the old atomically, after `UPDATE_IDLE` (10 min) without traffic.
- **Healthy or rolled back:** the new version is good once it has been healthy for `UPDATE_HEALTHY` (60 s). Otherwise it is rolled back after `UPDATE_MAX_STARTS` (3) failed starts.
- **Version skew:** a node whose peer runs a newer major version shows `peer_newer`.

### 22.4 Config

`config.json` lives in the state directory: `/var/lib/wirebond`, `/Library/Application Support/wirebond`, or `%ProgramData%\wirebond`. It is owned by root/SYSTEM, mode 0600.

```
{"v": 2, "role": "client", "anchor": "xanth", "mode": "performance",
 "anchors": ["xanth", "vps"], "allow": [], "support": null,
 "stun": ["stun.l.google.com:19302", "stun.cloudflare.com:3478"],
 "bond_net4": "10.77.0.0/24", "bond_net6": "fd77:b0d::/64", "mesh": ["100.64.0.0/10", "fd7a:115c:a1e0::/48"]}
```

`bond_net4` and `bond_net6` can change if a local network already uses 10.77.0.0/24. A Windows anchor ignores `bond_net4`: ICS fixes 192.168.137.0/24. `mesh` lists the mesh's ranges for meshes other than Tailscale.

### 22.5 Build

- **Release profile:** `opt-level` 3, fat LTO, one codegen unit, `panic = "abort"`, stripped.
- **Binaries:** static musl on Linux. The daemon was 1.8 MB in 1.x and should stay near that, plus Wintun on Windows.
- **Dependencies:** the daemon keeps 1.x's set (aes-gcm, hmac, sha2, zeroize, serde, serde_json, socket2, getrandom, libc, windows-sys). The TUN, routes, NAT and DNS code adds no crate. The viewer's dependencies (GPUI) stay out of the daemon.

## 23. Testing and acceptance

### 23.1 Tiers

| Tier | What | Where |
| --- | --- | --- |
| 1 | `cargo test`: units, vectors, pair tests of the engine | Linux, macOS, Windows CI |
| 2 | `netsim`: deterministic acceptance on simulated links and traces | Linux CI; PR set on every push, full set nightly |
| 3 | System harness: network namespaces for client, anchor and an "internet" with a server, with real TUN devices, nftables NAT, tc shaping and netem jitter; driven by `crates/load` | Linux CI |
| 4 | Field run: the owner's Mac at a hotel (Wi-Fi plus an iPhone on USB) bonded to xanth | by hand, before 2.0.0 ships |

### 23.2 `crates/load`

A UDP echo with a paced sender at fixed rate and size, reporting:
- loss;
- p50, p99 and p99.9 delay;
- the longest gap;
- frames when shaped as video: decodable frames, unrecovered frames, frame-delay percentiles.

It also runs TCP bulk transfers (throughput) and samples CPU and RSS of the daemons. It prints JSON for CI and tables for people.

### 23.3 Acceptance criteria

Scenarios run in netsim and, where marked, in the harness. They use 30 Mbit/s video at 120 frames per second, hotel Wi-Fi at 38 Mbit/s, 30 ms one way, with a 64 KB burst and a 180 ms buffer, and a phone 8 ms slower at 80 Mbit/s with 0.5% random loss.

| ID | Scenario | Criterion |
| --- | --- | --- |
| A1 | Performance, hotel spikes of 10–50 ms every 2–5 s | p99 frame delay ≤ the hotel's p50 + 10 ms; worst frame ≤ p50 + 15 ms; at most 1 unrecovered frame per 5 min |
| A2 | Performance, the owner's recorded hotel jitter | as A1, and the worst frame at least 3× lower than the baseline's (mesh lane only) |
| A3 | Performance, clean hotel | median within 1 ms of the hotel alone; no frame lost |
| A4 | Bandwidth, both links clean, bulk TCP | throughput ≥ 0.9 × (hotel + phone) − overhead; reorder wait p99 ≤ base skew + 10 ms |
| A5 | Either mode, hotel link pulled mid-stream | no gap over 130 ms; no flow ends |
| A6 | Either mode, the hotel drops full-size packets | traffic moves to the phone lane within 1 s; no flow ends |
| A7 | Bond down for 10 s (harness) | fail open within `FAIL_OPEN_AFTER` + 1 s; capture back within 3 s of a lane validating |
| A8 | Client → internet server through anchor NAT (harness) | TCP and UDP work, IPv4 and IPv6; a forged source from the client is dropped |
| A9 | Client → service on the anchor at 10.77.0.1 (harness) | delivered locally; never NATed |
| A10 | `pick` with two candidates (harness) | picks the one with the better score in each mode; `--dry-run` changes nothing |
| A11 | Every scenario | never worse than the baseline at p50, p99 and p99.9 (performance), or in throughput (bandwidth) |
| A12 | CPU at 100 Mbit/s in performance with two lanes (harness) | ≤ 25% of one core per daemon |
| A13 | Sleep and wake, a link change, the mesh down for 60 s | the bond recovers without restarting the daemon |

CI jobs: format, lint, the core-purity and dependency allow-list checks, tier 1 on all three OSes, tier 2 (PR set; nightly full), tier 3 on Linux, and the release-target builds.

## 24. Decisions

| Decision | Rejected | Why |
| --- | --- | --- |
| The anchor is the merge point and the exit | racing or adding the client's direct path | a server answers one address; copies must meet at a point the user controls |
| Capture all traffic through a TUN | 1.x's per-port loopback forwards | no per-app setup; Moonlight is just the anchor as destination |
| Kernel NAT on the anchor (nftables, pf, ICS on Windows) | a userspace TCP/IP stack (smoltcp or a netstack); WinNAT | far less code and no new crate. WinNAT needs Hyper-V, which Windows Home lacks |
| Bond addresses (10.77.0.0/24) for the anchor's own services | routing the anchor's mesh address into the bond | the control channel must stay on the mesh, and excluding one port from a route is not portable |
| Fail open when the bond is lost | fail closed | wirebond is for steadiness, not privacy; a lost bond must not leave the client offline |
| Performance copies everything; no switching | 1.x `stable` (one primary, failover, trials) | in netsim a reacting design let 89–268 ms frames through, against 45 ms with copies |
| Two modes per bond | Speedify-style per-flow redundancy | small flows are already always copied; a stream session has one bulk flow, so per-flow equals per-bond, and reacting lets the first spike through |
| No data caps | caps on metered links | the user buys steadiness with data on purpose |
| `pick` on demand | background anchor selection | predictable; moving a running connection to another anchor breaks it |
| Supports on cables only | wireless supports | a wireless hop shares the client's radio |
| RustCrypto `aes-gcm` | ring, aws-lc-rs | 2× faster per packet, but they need a C toolchain and break the ARMv6 musl build; at 60 Mbit/s crypto costs at most 0.35% of a core |
| One-thread poll loop | an async runtime | enough at these rates, smaller binary, deterministic |
| GPUI viewer, separate binary | Lynx for 2.0 | Rust-native and runs on Linux, which Lynx's desktop support (macOS and Windows since 3.7) does not; Lynx waits for a phone viewer |
| Keys over the mesh's encrypted channel | a handshake of our own | reuses the mesh's trust; about 135 lines with the ratchet |

## 25. Removed from 1.x

The code goes:
- Port forwards, `serve` lists, the Moonlight port preset, TCP splices, fallback flows and their constants (`MAX_SPLICES`, `FALLBACK_HOLD`, `FALLBACK_AFTER`).
- `stable`, `combine` and `capacity`: code, names and wire values; `FITS_HOLD`, `FITS_MARGIN`, `TRIAL_PASS`; the simulator's "primary" and "trial" roles.
- The macOS login agent for clients: every role is now a system service.
- The Go load tool (`tests/system/load`) and `go.mod`: replaced by `crates/load`.

The documents go:
- `OUTLINE.md`, which justified 1.x's Go lines.
- The Go-era notes: `notes/core.md`, `node.md`, `host.md`, `update.md`, `netsim.md`, `cmd.md`, `tooling.md`.
- `crates/*/SKELETON.md` and `design/`.
- `notes/rust.md` and `NOTES.md`, rewritten for 2.0.

`RESEARCH.md` and `CONTEXT.md` stay as background with citations.

## 26. Later

- `pick` on a schedule or on network change.
- A phone viewer (Lynx), reading `view` over the mesh.
- `recvmmsg`/`sendmmsg` and UDP GSO/GRO, once rates above about 500 Mbit/s matter.
- A Windows anchor with IPv6 egress, through a userspace stack.
- ChaCha20-Poly1305 for anchors and supports without AES instructions (Raspberry Pi 1 and Zero).
- More than one bond per client (one per anchor, chosen per destination).
