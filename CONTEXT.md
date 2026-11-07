# wirebond context

Background for whoever builds wirebond from [SPEC.md](SPEC.md). Three documents go together: SPEC.md says what to build, this file says why (the owner's setup, what was measured and what was decided), and [RESEARCH.md](RESEARCH.md) holds the evidence. SPEC.md cites RESEARCH.md throughout, so RESEARCH.md is committed next to SPEC.md and CONTEXT.md in the same change. A citation such as `[Gaming 5]` means finding 5 of a RESEARCH.md section, as SPEC.md's citation table lists.

## The owner and the goal

The owner streams their home Windows PC to a MacBook with Sunshine and Moonlight, to work while travelling and to play Overwatch. On a Marriott's premium Wi-Fi the stream dropped about 20% of its frames.

The owner's direction, 2026-11-05: "We don't have to reinvent the wheel. We can piggyback off mesh network tools like Tailscale already. All we want is a tool that utilizes that mesh to enhance a specific connection." It overrides the earlier goal of a network of wirebond's own.

- Tailscale, or a mesh like it, supplies identity, trust, discovery, NAT traversal, the control channel and one path. wirebond adds paths and scheduling for one chosen connection.
- The first user is Moonlight, from the Mac to xanth.
- One-line install, self-update, no leftovers. Lean and elegant above all.
- Use other mesh devices, phones included, only where they truly help.

The owner judges the design on six axes:

| Axis | Means |
| --- | --- |
| Ease of use | For humans and for agents. |
| Robustness | Never worse than the mesh alone. |
| Performance | Competitive-gaming stability by default; a throughput mode on request. |
| Low complexity | Fewest moving parts. |
| Small size | Few lines, one dependency, a small binary. |
| Elegance | Prefer removing a feature to adding a flag. |

By default (`stable`), the lowest-delay path that carries the stream carries it; the others protect it. `combine` adds capacity. This changes the earlier rule "the connection with the most capacity carries the traffic" (SPEC R7); the owner confirmed it on 2026-11-05.

The repository is <https://github.com/TristanCopley/wirebond>. `main` holds the first spec; work happens on `spec/next` and branches from it.

## The devices

| Device | Role | Details |
| --- | --- | --- |
| **xanth** | Home PC, stream host. Runs `wirebond serve moonlight` as a system service started at boot, so it serves after an unattended reboot with nobody logged in. | Windows, RTX 4090, Sunshine as a Windows service. Two 4K monitors side by side; Sunshine captures the right-hand one (240 Hz, HDR on). Wired Ethernet on a UniFi router, 1 Gbps internet. Tailscale name `xanth`; WSL appears as `xanth-wsl`, the same host. Tailscale needs Run unattended turned on (tray › Preferences, or `tailscale up --unattended`); without it the mesh is down while nobody is logged in [Tailscale KB 1088]. |
| **MacBook** | Travel laptop, stream client. Runs `wirebond connect xanth` as a login agent, no sudo. | MacBook Pro, M1 Pro (no hardware AV1 decode, so HEVC). macOS 26, application firewall on; it may need one `socketfilterfw` command. macOS Local Network privacy applies to a login agent, not to a tool run from Terminal. Spike S0 decides by running its client as a LaunchAgent; if prompts return after every self-update, the Mac client becomes a LaunchDaemon installed once with sudo. Tailscale name `matthews-macbook-pro`. Moonlight 6.2.0. Keep Wi-Fi above iPhone USB in the network service order, or all of the Mac's traffic, Tailscale's included, moves to the metered phone. |
| **iPhone** | Cellular hotspot, a link on the Mac. Nothing installed. | T-Mobile US, IPv6-only with 464XLAT: the phone's cellular IPv4 is a `192.0.0.x` CLAT address no peer can reach [Phones 9]. Personal Hotspot gives the Mac a `172.20.10.0/28` network, over Wi-Fi or USB; wirebond labels it `iPhone` and treats it as metered. USB is the useful form: over Wi-Fi the Mac's single radio leaves the hotel network. Tailscale name `iphone181`. wirebond on the iPhone itself is not planned: one VPN slot, sleep after lock, no self-update [Phones 1, Phones 2, Phones 13]. |
| **Raspberry Pi** | Optional Tailscale peer relay (`tailscale set --relay-server-port`). | On xanth's LAN it shares xanth's uplink, so a wirebond relay through it adds nothing. |

**The home line.** Its type (PPPoE or not) and whether it has IPv6 are unknown. Both matter: lane packets are sized to fit PPPoE's 1,492 bytes, and IPv6 would let the T-Mobile lane skip carrier NAT.

**The router.** It forwards UDP 41641 to xanth, so Tailscale connects directly. It should also forward UDP 7849 to xanth: recommended, and required only if `status` reports `tailscale_only`. The owner makes router changes themselves.

## What was measured

From the MacBook on 2026-11-04 and 2026-11-05, travelling near Chicago; home is near Los Angeles.

**Distance.** Tailscale relay latency from the Mac: Chicago 35 ms, Los Angeles 91 ms. The distance alone costs about 57 ms of round trip.

**iPhone hotspot.**

| Hop | Round trip |
| --- | --- |
| Mac → iPhone | ~5 ms |
| Mac → 1.1.1.1 | 27–30 ms |
| Mac → xanth over Tailscale (direct) | 84–116 ms, ~95 ms average |

Moonlight's statistics overlay on the hotspot showed 114 ms network latency with 23 ms variance; everything else was tiny (host processing 1.5 ms, decoding 4 ms, frame queue 1 ms, rendering 1.4 ms). The carrier changed the Mac's public address between checks (carrier-grade NAT), and Sunshine's log showed the client reconnecting every few minutes. **The hotspot's throughput is not measured.** The owner reports it carried Moonlight at 80 Mbps (1080p, 120 fps) without drops, so SPEC §17.3 sets cell = 80 Mbps, unmeasured. Measure with `networkQuality -I <iPhone interface>` before relying on it.

**Marriott premium Wi-Fi.**

| Check | Result |
| --- | --- |
| macOS `networkQuality` | 37.7 Mbps down, 41.9 Mbps up; idle 16.5 ms; under load 105 ms (down) and 196 ms (up) |
| 100 pings to 1.1.1.1 | 0% loss, average 32 ms, max 366 ms, deviation 60 ms |
| Wi-Fi link | 802.11ac, 5 GHz channel 40, **20 MHz** wide, −58 dBm signal, −94 dBm noise, 173 Mbps link rate; open network with a captive portal |
| Mac → xanth over Tailscale | direct, 64 ms |

Reading: a per-device shaper near 40 Mbps on a narrow channel shared with other guests. Moonlight at 80 Mbps sent twice what fits, so the shaper dropped the rest: the 20% frame loss. The interim workaround is Moonlight on HEVC at 30 Mbps.

**Why this shapes the design.** A short burst of test packets passes a token-bucket shaper at full radio speed, so the hotel lane would look like 100+ Mbps to a burst test while carrying only 38 Mbps of real traffic. So wirebond takes capacity from delivery-rate samples taken while its own pacer is backlogged (SPEC §8.3). Moving video back to a faster lane needs a 500 ms trial at the stream's rate, with the stream's own packets also sent on that lane, which a 64 KB shaper burst cannot fake (SPEC §11.1 rule 5).

**Jitter.** netsim replays the hotel's measured ping distribution (deviation 60 ms, max 366 ms), not ±8 ms [Congestion 6].

## Moonlight and Sunshine

Checked against moonlight-common-c, moonlight-qt and Sunshine source by reviewers.

- **Incomplete frames are dropped.** `RtpVideoQueue.c` purges a frame that is not complete when a packet of the next frame arrives, and rejects packets of earlier frames. There is no time-based reorder window. A frame lasts 8–17 ms at 120–60 fps, less than the delay difference between hotel Wi-Fi and cellular. Splitting one stream across lanes without reordering would lose most frames: so `stable` keeps bulk on one lane, and `combine` reorders.
- **Frame repair.** Sunshine adds 20% Reed–Solomon parity per frame (`fecPercentage` 20) and sends each frame as a burst at about 0.8 Gbit/s, in batches up to 64 KB [Gaming 1, Gaming 10]. Each frame Moonlight cannot repair freezes video for about one RTT, until a reference-invalidation or IDR frame arrives [Gaming 2].
- **Input** rides reliable ENet channels, resent after RTT + 4 × variance, doubling [Gaming 3].
- Audio tolerates about 30 ms of reordering. The control channel (ENet) discards duplicates.
- **ENet (UDP 47999) drops packets from a changed source address**, so each flow keeps one socket on each end.
- **Ports** (base 47989): TCP 47984 HTTPS, 47989 HTTP, 48010 RTSP; UDP 47998 video, 47999 control, 48000 audio. Sunshine matches UDP streams to a session by ping payload, not by source address.
- **Sunshine's web UI (TCP 47990) is never forwarded.** Through a loopback forward it would see a localhost origin and pass its origin checks.
- **`LocalIP`.** Sunshine reports the address a request arrived on as `LocalIP` in `serverinfo` and in the RTSP session URL, and moonlight-qt polls that learned address before the one the user typed. So the wirebond server delivers to `127.0.0.1` on xanth, never to xanth's Tailscale address; otherwise Moonlight would learn that address and stream around wirebond.
- **Packet size.** With Moonlight pointed at `127.0.0.1`, it treats the host as LAN: moonlight-qt's `getActiveAddressReachability` classifies `lo0` (MTU 16,384, not point-to-point) as LAN. It then sends 1,392-byte video payloads, 1,408-byte datagrams with Sunshine's 16-byte RTP header. Plain Tailscale would get 1,024-byte payloads. wirebond adds 35 bytes, so lane packets are 1,471 bytes over IPv4 and 1,491 over IPv6, inside PPPoE's 1,492. On the Tailscale lane and through the fallback, each video datagram crosses Tailscale (MTU 1,280) as two fragments.
- **Pairing.** In Moonlight, Add PC `127.0.0.1` without deleting xanth. Moonlight finds the same unique id, folds the address into the xanth entry and keeps the pairing (moonlight-qt `PendingAddTask`, `NvComputer::update`). Deleting the entry drops its pinned server certificate, and pairing again needs a PIN in Sunshine's web UI, which wirebond never forwards.
- Sunshine's UPnP option stays off, so the router exposes no second, unbonded path.

## Decisions

| Question | Decision | Why |
| --- | --- | --- |
| Third-party bonding (Speedify) | No | Detours through its servers, needs a subscription, and stacks a second VPN under Tailscale. For Moonlight, xanth itself is the far end, so every server-based bonder adds a hop that wirebond does not [Landscape 3]. |
| Extra identities on the Mac's Wi-Fi card | No | Wi-Fi only delivers to an address that associates and acknowledges every frame in hardware within 16 µs; Apple's driver supports one client interface, and extra identities share one radio's airtime anyway. |
| Linux bonders (OpenMPTCProuter, Glorytun, engarde) | No | None runs on both macOS and Windows. |
| Where the bonding ends are | **The two devices themselves** | The sender splits, the receiver reassembles; no middle server, no detour. |
| How programs reach it | **A loopback port forward; Tailscale is the network** | The owner's direction (2026-11-05), replacing wirebond's own IPv4 overlay. No driver, no route, no admin on the client; the server delivers to `127.0.0.1`. A TUN for one address is the fallback design if spike S0 fails. |
| Trust and identity | **The tailnet's membership and ACLs** | Tailscale already authenticates every mesh address, so wirebond has no CA, invites or certificates. The server forwards only ports it shares. |
| Keys | **A random AES-256-GCM key per pair of processes, sent over the control channel inside Tailscale, ratcheted every 2^24 packets; a 16-bit key id in each header** | About 135 lines. wireguard-go needs IP packets from a TUN and would add about 7,300 lines [Mesh 2]. Keys never touch disk and survive a Tailscale outage. The ratchet keeps each key inside AES-GCM's usage limit (RFC 9001 §6.6) and gives forward secrecy per epoch. |
| Relay lanes through other devices | **No** | The Pi shares xanth's uplink; a phone relay stops on lock; Tailscale peer relays cover hard NAT at zero code [Landscape 2]. Revisit only for a device with an independent uplink. |
| Modes | **`stable` (default) and `combine`** | Named by outcome; gamers read "speed" as latency [CLI 14]. They replace the names "primary" and "aggregate". |
| Default rule (R7) | **The lowest-delay lane that carries the stream**, else the largest | Same result when the stream outgrows the faster lane; about 15 ms less one-way delay when it fits [Landscape 7]. Changes the earlier "most capacity" rule; to confirm with the owner. |
| Copying | **Cheap flows (under 1 Mbps) always copied, one lane per bottleneck group; bulk never mirrored** | Copies help only on an independent, lightly loaded lane [Gaming 6], so lanes not carrying bulk take copies only under 50% load. No owner setup has an unmetered independent lane with twice the stream's headroom, and 30 Mbps of video on the hotspot is 13.5 GB an hour; rewind covers stalls. |
| FEC or cross-lane parity | **None** | Sunshine's 20% in-frame Reed–Solomon covers random loss; surviving a stalled lane needs 100% parity, which is duplication [Gaming 9, Multipath 8]. |
| Queue management | **Hard 100 ms age cap, no CoDel** | Moonlight does not slow down on drops. |
| Don't Fragment | **Never set; no size check** | Over IPv4, routers with a smaller MTU fragment; over IPv6 no router does. A lane that drops full-size packets or their fragments shows as bulk loss: the overload rule moves video off it, and the return trial (or, in `combine`, a bar) keeps it off. |
| Fallback | **If the client's dials to the server have failed for 30 s, it relays flows straight to the device over Tailscale. If the server runs another protocol major, or no lane comes up within 3 s of a connect, only UDP flows take that route; TCP still rides port 7849** | Never worse than Tailscale alone. A new connection dials at once and waits up to 5 s, redialing every second, so a gap (an update restart, a reboot, a wake) never shows Moonlight xanth's mesh address. The `tcp` preamble never changes between majors, so a version mismatch never does either. The cost when the TCP fallback does happen: Moonlight may learn that address; `fallback_used` tells the owner. The client caches the device's shared ports (`seen`), so the fallback works after the Mac restarts while xanth's wirebond is gone. |
| Tailscale CLI and API | **Never called** | MagicDNS and interface addresses suffice; works on any mesh and on the App Store build. |
| Multipath TCP | **Off on every socket** (`godebug multipathtcp=0` in go.mod, checked by CI) | CVE-2026-98432 [Landscape 11]; one line covers listeners and dialers. |
| Size | **At most 2,500 shipped lines, `golang.org/x/sys` only, enforced in CI** | The owner's leanness requirement. |
| Release signing | Not now | The signing key would live in the same GitHub account that publishes releases, so it adds little over HTTPS plus checksums. An offline key fits the update flow later. Releases are 0.1–0.4 (SPEC §20). |
| Exit routing | Later | Not in the requirements and the largest optional surface; Tailscale's exit node covers the simple case. Any bonder must exit somewhere, and exiting at home adds about 57 ms round trip. |

## Security lessons

V1 was a Moonlight-only relay. Its review, and the design review of the drafts in `design/` and of SPEC.md, found these. All are folded into the spec.

- Never put a per-lane field in a header that is authenticated under a shared nonce: AEAD nonce reuse. wirebond has one counter per key for every packet type and lane; the nonce is the key id and that counter; per-lane and per-flow fields live inside the sealed payload.
- Never derive a nonce from anything but the key's own counter. A per-flow sequence number in the nonce reuses nonces when a flow id comes back under the same key (the gaming draft's flaw).
- Move a lane's return address only on an authenticated probe with a higher sequence number, never on data, or a replayed packet redirects traffic.
- Validate a moved address, as QUIC does, before it gets anything but probes. Moving a lane's return address on any authenticated probe lets an on-path network reflect the stream onto a victim (the design review found it).
- Put the lane id inside the sealed probe. A per-lane field outside the seal can relabel a probe and hijack a return address (the minimal draft's flaw).
- A replay window must be thousands of packets wide, not 64, or the slower lane's copies are dropped as replays.
- Check the replay window after the packet opens; only authenticated packets touch any state. A per-lane window comes before a lane's statistics, so a duplicated or replayed packet never counts twice.
- A key belongs to one pair of processes and is never stored, so a restarted side never accepts an old key.
- Deliver to `127.0.0.1`. Delivering to the host's own mesh address leaks it to Moonlight through `LocalIP` (the minimal draft's flaw).
- Forward only an explicit share list. Forwarding to any loopback port the client names exposes every localhost-only service to every tailnet device (the agent draft's flaw).
- Use Unix sockets with file permissions for the local API. A local API on `127.0.0.1` TCP can be reached by a web page.
- Sort STUN replies by source and transaction id before the packet parser sees them. They share the lane sockets, and a Binding success response starts with the data type's byte.
- Bound the work done before authentication. A receiver that derived the key for any epoch a header named could be made to chain about 2^40 HMACs by one forged packet; it accepts only the previous, current and next epoch (a review found it).
- Bound what one tailnet peer can hold: sessions per device, flows per session, TCP splices per peer, and one session per peer process. Unbounded flow sockets would exhaust Windows' ephemeral ports.
- Keep the control channel from outliving its peer. Go's default keepalive takes about 150 s to notice a rebooted server; wirebond uses about 11 s, and every `tcp` answer carries the server's `inst`, so a restart is noticed at the next Moonlight request.
- Re-bind sockets when the iPhone USB interface reappears with a new index.
- Send replies from the socket that received, or address-checking peers drop them.

## Owner preferences

- Lean, elegant, minimal. Prefer removing a feature to adding a flag.
- No installers, no package managers, no leftover files. Self-update in place.
- Explain trade-offs plainly, with numbers; say what is physics and what is software.
- The owner makes changes to network equipment and accounts (router port forwards, GitHub secrets, Tailscale settings). List them as steps; do not perform them.
- `wirebond serve` on Windows adds a firewall rule for its own executable and installs a system service: one admin step on xanth. The Mac needs no admin. To confirm with the owner.

## Open items for the builder

1. **Base branch.** The repository has no commits. Ask the owner before pushing an initial commit to `main` that the first pull request can target.
2. **Direction.** Confirm the Tailscale-piggyback direction and the R7 change (the lowest-delay lane that carries the stream) in the first pull request.
3. **Port forward.** Forward UDP 7849 on the UniFi router to xanth: recommended, required only if `status` reports `tailscale_only`. Owner step.
4. **Hotspot throughput.** Measure it, then tune against A2 and A4 (SPEC §17.3).
5. **Toolchain.** The owner's Mac has no Go and the owner declined installing it; build and test in the cloud environment.
6. **Real-world test.** The field acceptance runs of SPEC §17.5 need the owner running Moonlight between the Mac and xanth; coordinate them.
7. **Spike S0.** Run it (SPEC §20) before any lane code, with its Mac client as a LaunchAgent. Stop and redesign capture if `LocalIP`, Moonlight's address list after Add PC (pairing kept), or ENet through the per-flow socket fails.
8. **Unknowns.** Find out whether xanth's line is PPPoE and has IPv6, and T-Mobile's IPv4 NAT type.
9. **ACLs.** If the tailnet uses custom ACLs, allow TCP and UDP 7849 from the Mac to xanth. Owner step.
10. **Peer relay, optional.** A Tailscale peer relay on the Pi. Owner step.
11. **AWDL, optional.** Measure the Mac's AWDL gaps; netsim's AWDL model is a guess until then [Testing 7].
12. **Run unattended.** Turn on Tailscale's Run unattended on xanth. Owner step.
13. **Service order.** On the Mac, keep Wi-Fi above iPhone USB in System Settings › Network › Set Service Order. Owner step.
14. **Line limit.** Confirm the 2,500-line limit (R11) before 0.1. SPEC §19's itemised budget sums to exactly 2,500 with no headroom, after cutting the Linux login agent, `serve` without arguments, `update --check` and two firewall problems; reviews estimate the scope at 2,600–3,300 lines. Measure at 0.2 and 0.3.

## Open questions for the owner

### Answered on 2026-11-05

| # | Answer |
| --- | --- |
| Q1 | Yes: Tailscale is the network; wirebond enhances one connection. |
| Q2 | Yes: the `stable` rule stands. |
| Q3 | Acceptable, but the owner asked for an alternative. The builder lays out the no-admin option (a per-user login item on xanth, serving only after login, with the Windows firewall prompt answered once) next to the system service, and keeps the system service as the default. |
| Q5 | Not hard, but lower is better: the owner wants the line count reduced further. Treat 2,500 as a ceiling and aim well below it; every cut that keeps the acceptance tests is welcome. |
| Q6 | Done: `main` has the first commit. |

### Still open

Each needs the owner's answer or action; the builder asks, and does not decide alone.

| # | Question | Why it matters | Where |
| --- | --- | --- | --- |
| Q1 | Do you confirm the direction: Tailscale is the network, and wirebond enhances one connection through a loopback port forward? | It replaces the own overlay, CA, invites and TUN of the earlier design. | Goal; SPEC §3, §5 |
| Q2 | Do you confirm the `stable` rule: the lowest-delay lane that carries the stream carries it, else the largest? | It changes the earlier "most capacity carries the traffic" rule; same result when the stream outgrows the faster lane, about 15 ms less one way when it fits. | SPEC R7, §11.1 |
| Q3 | Is one admin step on xanth acceptable: a firewall rule for wirebond's executable and a Windows system service? | Without the service, xanth does not serve after an unattended reboot. | Owner preferences; SPEC §15.3 |
| Q4 | If spike S0 shows firewall or Local Network prompts return after every self-update, is a one-time `sudo` install of the Mac client as a LaunchDaemon acceptable? | The default Mac client is a login agent with no admin. | SPEC §20 item 6, §22 |
| Q5 | Is the 2,500-line limit firm? The itemised budget meets it with no headroom; reviews estimate the scope at 2,600–3,300 lines. If firm, the next cut is `view` and the server's half of `status` (about 40 lines). | CI enforces it; a feature that passes it must remove something. | SPEC R11, §19 |
| Q6 | May the builder push an initial commit to `main` so the first pull request has a base? | The repository has no commits. | Open item 1 |
| Q7 | Is xanth's home line PPPoE, and does it have IPv6? | Lane packets are sized for PPPoE's 1,492 bytes; IPv6 lets the T-Mobile lane skip carrier NAT. S0 item 5 checks IPv6 and the T-Mobile NAT type if the owner does not know. | SPEC §6.3, §20 |
| Q8 | Does the tailnet use custom ACLs? If so, will you allow TCP and UDP 7849 from the Mac to xanth? | The default policy allows it; a custom one that blocks UDP 7849 sends UDP flows to the fallback (`lanes_blocked`). | SPEC §5.3, §16 |
| Q9 | Will you forward UDP 7849 on the UniFi router to xanth? | Recommended; required only if `status` reports `tailscale_only`. | SPEC §6.3, §16 |
| Q10 | Will you turn on Tailscale's Run unattended on xanth, and keep Wi-Fi above iPhone USB in the Mac's service order? | Without the first, the mesh is down at boot with nobody logged in; without the second, all of the Mac's traffic moves to the metered phone. | SPEC §16 |
| Q11 | Will you measure the hotspot with `networkQuality -I <iPhone interface>` before tuning? | The 80 Mbps figure is reported, not measured; A2 and A4 depend on it. | SPEC §17.3, §22 |
| Q12 | Do you want the optional steps: a Tailscale peer relay on the Pi, and a measurement of the Mac's AWDL gaps? | The relay helps only if both ends are behind hard NAT; the AWDL model in netsim is a guess until measured. | SPEC §6.4, §16, §22 |
| Q13 | When can you run the field acceptance (SPEC §17.5) at a hotel with the iPhone on USB? | Emulation is at best within about 17% of reality; the field run is the final gate. | SPEC §17.5 |
| Q14 | When a stream fits no single lane in `stable` (say 60 Mbps against a 38 Mbps hotel and a hotspot that measures under 60), should `stable` spread it across lanes on its own, as `combine` does, until one lane fits again? | Today `stable` keeps it on one overloaded lane and `bulk_exceeds_every_lane` asks you to run `wirebond connect xanth combine` or lower the bitrate. An automatic spill needs about 15 lines and makes `stable` partly a second `combine`. | SPEC §11.1, §21 |
| Q15 | Is it acceptable that a Linux device runs wirebond only as a system service, installed with `sudo`? | It removes the Linux login agent, its state directory and `loginctl enable-linger`; no owner device runs wirebond on Linux today. | SPEC §15.3 |
