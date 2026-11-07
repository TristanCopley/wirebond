# wirebond research

Eight research threads, run 2026-11-05. Each has a summary, findings with evidence and the implication for wirebond, and proposals.


## Multipath scheduling and bonding

### Summary

1. Primary mode as the default is well supported. Apple has kept MPTCP Aggregate developer-only from iOS 11 (2017) to today. Deng et al. (IMC 2014) found that picking the right single path cut response time by 50%, against 35% for MPTCP, and that LTE beat Wi-Fi 40% of the time. Peplink's own whitepaper says to bond only links within 150 ms latency of each other and to use slower links for failover. Converge (SIGCOMM 2023) found naive multipath real-time video can do worse than single path.

2. SPEC §9.1 has a real flaw. Step 4 copies bulk traffic to the hedge lane. Moonlight discards an incomplete frame as soon as a packet of the next frame arrives. When the faster primary is losing packets, the missing packets arrive on the slower lane after the next frame has started (hotel to cell skew is about 15 ms; a frame lasts 8.3 ms at 120 fps). So hedging fixes nothing for video and reorders packets, which contradicts step 7's claim. Video stays broken until promotion, which takes at least 3.5 s. A clean switch fixes this: moving to a slower lane never reorders, and moving to a faster lane needs a one-time wait equal to the OWD difference.

3. On Wi-Fi the worst latency comes from delay spikes, not loss. In AUGUR's production data (Tencent cloud gaming, NSDI 2024), about 60% of the packets in stalled frames arrived without loss, and the path RTT was already above 200 ms before most stalls. Re-sending in-flight frames on cellular cut p99 latency by 45.8% for 0.2% more cellular data. Switching the primary to cellular cut it by another 25%. wirebond can copy this without acks: on a switch, resend the sealed packets recently sent on the old lane from a small ring buffer, and let the replay window drop duplicates.

4. Cheap redundancy for small packets is the most reliable latency tool. ReMP halved mean RTT and cut its deviation 19×. Siri got 20% faster at p95 with 5× fewer failures. Peplink duplicates TCP ACKs, and Speedify sends redundant copies on up to 3 links. wirebond should send urgent packets on every up lane, with a budget cap.

5. Earliest-arrival, the rule in SPEC §9.2, belongs to the scheduler family that wins in evaluations. ECF and minRTT came out on top in a 2025 study. Round-robin was worst, and BLEST underused the slow path when paths differed a lot. ECF's kernel code adds a jitter margin and switching hysteresis that §9.2 lacks.

6. Aggregate mode needs a bound on its own queues. Peplink's DWB drops packets early to avoid bufferbloat and treats a link as congested at 2× its idle latency. §9.2's pacing queues have no limit, so TCP inside the overlay will fill them.

7. One ordering counter per peer means a loss in one flow holds up every other flow (cross-flow head-of-line blocking, the problem QUIC streams solve). Adding a 4-byte pointer to the previous packet of the same flow removes it, and the receiver keeps no per-flow state.

8. FEC is not worth adding. Sunshine already sends 20% Reed-Solomon parity per video frame (config.cpp default). Delay spikes are not loss. Peplink itself says duplication works more reliably than FEC for video on the move.

9. Multipath QUIC (draft-21, submitted to the IESG) leaves scheduling out of scope. It does allow ACKs on any path, which would make wirebond's liveness detection independent of each lane's return direction.

10. Every production system treats cellular data as a cost, and the SPEC does not. At 80 Mbps the iPhone lane uses about 36 GB per hour.

### Findings

#### 1. SPEC §9.1 hedging (step 4: copy bulk to a second lane while the primary is congested) does not protect Moonlight video. It also reorders packets, which contradicts step 7's claim that only failover reorders.

**Evidence.** Moonlight discards an incomplete frame as soon as a packet of the next frame arrives (moonlight-common-c src/RtpVideoQueue.c, the 'Unrecoverable frame' path; CONTEXT.md). Hotel scenario: hotel OWD 30 ms (primary, about 50% loss at 80 Mbps through a 38 Mbps shaper); cell OWD 45 ms. The first copy wins. For frame N, hotel delivers its surviving packets at t+30 and the missing ones arrive through cell at t+45. Frame N+1's hotel packets arrive at t+38.3 (8.3 ms frames at 120 fps), so frame N is purged. Losses beyond Sunshine's 20% FEC (Sunshine src/config.cpp: fecPercentage 20) cannot be recovered. Video stays broken through the whole hedge until promotion: 500 ms hold plus 3 s, at least 3.5 s each time conditions change. This is my own analysis from the source, not measured.

**Implication.** Avoid: never split a bulk flow's first copies across lanes whose skew exceeds a frame interval. Adopt move-and-verify switching: bulk goes on exactly one lane at a time. Switching to a slower lane never reorders. Switching to a faster lane reorders at most ΔOWD of packets, avoided by holding the new lane's first packets for ΔOWD once. Promotion becomes the trial succeeding, which takes about 1.5 s instead of at least 3.5 s.

#### 2. Wi-Fi tail latency comes mostly from delay spikes, not loss. The measured fix is to resend in-flight data on the other path when switching, plus a temporary primary switch. It costs almost no cellular data.

**Evidence.** AUGUR, NSDI 2024, Tencent START production (https://www.usenix.org/system/files/nsdi24-zhou-yuhan.pdf). The wireless hop raises p99 frame latency by up to 290%. Path RTT was above 200 ms before the stall for 65% (2.4 GHz) and 50% (5 GHz) of stalled frames. Nearly 60% of packets in a stall arrived with no loss. Re-sending frames on cellular cut p99 latency by 45.8% and stalls by 55.5%, for 0.2% more cellular data. Switching the primary path cut another 25% (p99) and 31.7% (stalls), for 0.4% more. AUGUR probes the backup path with 8-byte pings every 50 ms. Overall cellular use was 2.7%. XLINK (SIGCOMM 2021, https://conferences.sigcomm.org/sigcomm/2021/files/papers/3452296.3472893.pdf) is consistent: gated re-injection cut overhead from 15% to 2.1%, with p99 chunk time 19–50% better.

**Implication.** Adopt a rewind on switch. Keep each lane's recently sent sealed packets in a ring buffer, about 300 ms or a byte cap, which is about 3 MB at 80 Mbps. On failover or a switch, resend on the new lane, at its pacing rate, the packets sent on the old lane within the last suspect-timeout + RTT. The 8,192-wide replay window already drops the late copies, and the identical sealed bytes need no new crypto. During a delay spike (not loss), the rescued frame arrives before the next frame's packets, which are stuck behind it on the old lane. That is why this works for Moonlight where retransmission after a loss would not.

#### 3. Primary or single-path as the default, with selective use of the other paths, is what the evidence and the shipped products converge on. Full aggregation is either not shipped or only for bulk.

**Evidence.** Apple (IETF 99 slides, https://www.ietf.org/proceedings/99/slides/slides-99-mptcp-sessa-ios-linux-implementation-updates-00.pdf): Handover 'minimizes cell usage'; Interactive is 'for latency-sensitive, low-volume flows'; Aggregation is 'only for developers, looking for good use-cases' and has stayed developer-only since iOS 11. Apple 2020 slides (https://datatracker.ietf.org/meeting/interim-2020-quic-02/materials/slides-interim-2020-quic-02-sessa-multipath-transports-at-apple-00.pdf): Siri does per-packet scheduling on the best path with an RTT threshold. Apple Music turns on cellular 'only when really necessary', then schedules aggressively on both paths. Deng et al. IMC 2014 (https://dspace.mit.edu/handle/1721.1/99718): LTE beat Wi-Fi 40% of the time; the best single path cut response time by 50% against 35% for MPTCP; MPTCP did worse on short flows. Converge, SIGCOMM 2023: naive multipath WebRTC can do worse than single path (abstract-level).

**Implication.** Adopt: keep primary as the default and aggregate as the toggle. In aggregate, consider Apple Music's pattern: add lanes only when the primary cannot carry the offered load.

#### 4. Duplicating small, latency-critical packets is the cheapest and most reliable latency tool. Production systems copy them to all links (up to 3), not 2.

**Evidence.** ReMP TCP (ICC 2016 abstract, https://www.aminer.org/pub/57d06416ac4436735429c7e7/remp-tcp-low-latency-multipath-tcp): about half the average RTT and 19× lower RTT deviation in a stressed mobile test. Siri over MPTCP: p95 time-to-first-word 20% faster and 5× fewer network failures (Apple IETF 99 slides). Peplink DWB duplicates TCP ACKs because they are small (SpeedFusion whitepaper ch. 12, https://www.venntelecom.com/documents/13/SpeedFusion_Whitepaper_ver04-02.pdf). Speedify Redundant sends each packet on up to 3 connections by default (https://support.speedify.com/article/870-bonding-mode). engarde duplicates everything on every link (https://github.com/porech/engarde).

**Implication.** Simplify and strengthen SPEC §9.1 step 1 and the §9.2 urgent rule to 'urgent packets go on every up non-fallback lane'. Add a per-peer byte budget, about 2 Mbps; above it urgent packets are treated as bulk, so a flood of small packets cannot multiply load or cellular data.

#### 5. The earliest-arrival rule in §9.2 is the right family: ECF, DAPS and STTF all lead in evaluations. The best implementation adds a jitter margin and switching hysteresis, which §9.2 lacks.

**Evidence.** ECF kernel code (https://raw.githubusercontent.com/multipath-tcp/mptcp/mptcp_v0.96/net/mptcp/mptcp_ecf.c) uses delta = max(rttvar_fast, rttvar_slow) in both inequalities, plus a beta and switching_margin hysteresis. The ECF paper (CoNEXT 2017) reports it 'never does worse' than default for web. A 2025 evaluation (https://arxiv.org/abs/2511.14550) found ECF and minRTT with BBR/OLIA gave the best goodput and per-packet delay in almost all scenarios. Round-robin was worst. BLEST underused paths in severe heterogeneity. DAPS (ICC 2014, simulation, https://hal.archives-ouvertes.fr/hal-01062851) cut receiver buffer occupancy by 77% and application delay by 63% against CMT-SCTP, but assumes quasi-static delay.

**Implication.** Measure and adopt: arrival(l) = OWD(l) + 2·dev(l) + (Backlog(l)+size)/Rate(l). A packet goes on a slower lane only if it wins by a margin. Prediction error on spiky Wi-Fi is what fills the reorder buffer, and the margin trades a little throughput for less reordering.

#### 6. Commercial bonders bound skew and queues explicitly: a skew limit for bonded links, a jitter buffer of about 150 ms, congestion at 2× idle latency, and active packet drops to prevent bufferbloat. §9.2's pacing queues have no size limit or AQM, so TCP inside the overlay will fill them.

**Evidence.** SpeedFusion whitepaper (https://www.venntelecom.com/documents/13/SpeedFusion_Whitepaper_ver04-02.pdf). Recommends bonded links 'within 50%' bandwidth and 'within 150ms' latency, and setting high-latency links as failover. DWB counts a WAN as congested when latency exceeds 2× idle (30 to 60 ms; Low 45, High 75). It 'can monitor latency change and avoid bufferbloat issues by dropping packets'. Packet Jitter Buffer defaults to 150 ms maximum. The tunnel is 'equalized to the highest latency link', so losses on the fast link take longer to detect. Forum: Receive Buffer 'will introduce extra latency to the tunnel' (https://forum.peplink.com/t/broadcast-video-layer2-speed-fusion/15963).

**Implication.** Adopt: bound every aggregate pacing queue with CoDel (RFC 8289, target 5 ms, interval 100 ms), and CE-mark inner ECT packets instead of dropping them (RFC 6040). Exclude from aggregation, keeping it supplemental, any lane whose base OWD exceeds the fastest lane's by more than about 50 ms. The 50 ms reorder cap can stay: it is stricter than Peplink's 150 ms, which suits gaming.

#### 7. A single ordering counter per peer (oseq) causes cross-flow head-of-line blocking. A lost packet of a background download holds Moonlight video and audio for up to 50 ms.

**Evidence.** SPEC §9.2: 'Type 0x04 packets from a peer are delivered in oseq order.' MPTCP's connection-level in-order delivery is the classic source of HoL (BLEST, IFIP Networking 2016, https://dl.ifip.org/db/conf/networking/networking2016/1570234725.pdf). draft-ietf-quic-multipath-21 §5.5: in-order data 'will experience the maximum delay of all used paths'. QUIC avoids cross-flow HoL with per-stream ordering.

**Implication.** Adopt a minimal fix: type 0x04 also carries prev (u32), the oseq of the previous packet in the same sender-side flow bucket. The receiver releases a packet once prev is delivered or declared lost, tracked in a bitmap over the oseq window. The existing loss rule stays, and the receiver keeps no per-flow state. Cost: 4 bytes per packet, 0.3%.

#### 8. wirebond should not add FEC. Moonlight already carries per-frame Reed-Solomon parity, the main Wi-Fi impairment is delay rather than loss, and even Peplink prefers duplication for video on the move.

**Evidence.** Sunshine src/config.cpp: fecPercentage default 20 (https://raw.githubusercontent.com/LizardByte/Sunshine/master/src/config.cpp, line 842). Peplink FEC costs 13.3% (Low) or 26.7% (High); Adaptive costs 6.7–20%. The whitepaper says WAN Smoothing 'tends to work more reliably on its own' when 'streaming video on the move'. AUGUR: about 60% of packets in stalled frames had no loss.

**Implication.** Simplify: state in the SPEC that wirebond has no FEC, and why. Cross-lane duplication (urgent packets, rewind) covers what FEC would.

#### 9. Multipath QUIC standardizes the mechanisms but leaves scheduling local. One mechanism wirebond lacks is acknowledging on any path, which separates forward liveness from a lane's return direction.

**Evidence.** draft-ietf-quic-multipath-21 (17 March 2026, submitted to the IESG; https://datatracker.ietf.org/doc/draft-ietf-quic-multipath/). Scheduling is out of scope. PATH_STATUS_BACKUP means no traffic if another path is usable. PATH_ACK 'could be received through any open path', and RTT may be taken as 'the sum of two one-way delays'. Keep-alives are not recommended by default. In SPEC §7.3 a lane is suspect when its own echoes stop, so a failure only in the return direction (A2b) looks like a forward failure.

**Implication.** Adopt cheaply: every probe carries EchoSeq and EchoDelay for every lane of that peer (about 20 bytes each, 1–3 lanes). A lane's forward health is then judged from echoes that return on the fastest lane, detection time becomes OWD_fwd + OWD_fastest_return, and A2b passes by construction.

#### 10. Every production multipath system treats cellular data as a cost. The SPEC does not, and in the owner's case the iPhone lane becomes primary at 60–80 Mbps.

**Evidence.** Apple Handover 'minimizes cell usage'; Apple Music turns on cellular 'only when really necessary'. AUGUR uses 2.7% cellular and reports user complaints about cellular use. Peplink 'Traffic Overflow' fills cheap links first (whitepaper §4.6). Arithmetic: 80 Mbps × 3,600 s ÷ 8 = 36 GB per hour; 30 Mbps is 13.5 GB per hour. Hotspot allowances are often tens of GB; plan specifics are not verified here.

**Implication.** Measure and expose: count bytes per link in status. Decision for the owner: R7 as written always picks the largest lane. A metered-aware rule would pick the unmetered lane that carries the offered load, and the largest lane otherwise. The iPhone hotspot can be detected from its 172.20.10.0/28 subnet with no flag.

#### 11. Idle keep-alive probes every 5 s on a cellular or hotspot link keep the radio in its high-power state. This matters once phones are devices on the mesh, and for the iPhone tethering the Mac.

**Evidence.** Huang et al., MobiSys 2012 ('A Close Examination of Performance and Power Characteristics of 4G LTE Networks'): LTE has a long high-power tail after each transfer, about 1,080 mW tail power (secondary lecture summary), and is up to 23× less power-efficient than Wi-Fi. The exact LTE tail duration was not verified here. RFC 4787 REQ-5 requires UDP NAT mappings to last at least 2 minutes, but carrier CGNAT may time out sooner (unverified).

**Implication.** Measure first: test the T-Mobile CGNAT UDP timeout with a probe-interval sweep. Then set the idle probe interval on metered links just under it, for example 25 s, rather than 5 s. Keep the fast 50 ms cadence only while data flows.

#### 12. Adaptive systems decide per flow whether to duplicate or split, based on loss and latency. They do not keep one global behavior.

**Evidence.** Speedify's default is Speed mode plus Enhance Streaming. For detected streaming flows it 'intelligently switches between active bonding and redundancy' (https://support.speedify.com/article/870-bonding-mode). Per the forum (anecdotal), Peplink DWB turns on FEC or WAN Smoothing on demand when it sees loss or latency. Apple Interactive is 'RTT-based with threshold' and 'adapts if retransmission timeout is too large' (2020 slides).

**Implication.** wirebond's existing urgent/bulk split by flow bucket is the parsimonious version of this. Keep it, and do not add a third 'streaming' class. The trial-switch and rewind mechanisms give bulk flows the adaptivity.

### Proposals

- §9.1 steps 4–5: replace bulk copy-hedging with a trial. When the primary has been congested for 500 ms, move all bulk to the candidate lane (urgent copies stay on every lane). If the candidate stays clear for 1 s it becomes primary; if it congests, bulk moves back and the candidate is barred for 60 s.
- §9.1: when bulk moves to a lane with a lower OWD, hold that lane's first bulk packets once for the OWD difference (at most 50 ms), so a switch never reorders. Moving to a slower lane needs no hold.
- §9.1 step 7: delete the claim that hedging cannot reorder, and state that bulk always travels on exactly one lane in primary mode.
- §9.1 step 7: keep each lane's last 300 ms of sealed packets in a byte-capped ring buffer. On failover or a trial switch, resend on the new lane, paced at its rate, those sent on the old lane within the last suspect-timeout plus RTT. The replay window drops the late copies.
- §9.1 step 1 and §9.2: send urgent packets on every up non-fallback lane. Cap each peer's urgent traffic at 2 Mbps with a token bucket; packets over the cap are treated as bulk.
- §9.2: change the arrival estimate to OWD(l) + 2·dev(l) + (Backlog(l)+size)/Rate(l), and send on a slower lane only when it beats the fastest lane's estimate by a margin, as ECF does.
- §9.2: bound each pacing queue with CoDel (target 5 ms, interval 100 ms), CE-marking inner packets that are ECN-capable instead of dropping them.
- §9.2: do not aggregate over a lane whose base OWD is more than 50 ms above the fastest lane's; it stays supplemental and carries urgent copies.
- §8.1/§9.2: type 0x04 carries prev (u32), the oseq of the previous packet in the same sender flow bucket. The receiver holds a packet only until prev is delivered or declared lost, so one flow's loss no longer delays others.
- §9 or §15: state that wirebond has no FEC, because Sunshine already adds 20% Reed-Solomon parity per frame and Wi-Fi tails are delay rather than loss.
- §7.3: each probe carries EchoSeq, EchoDelay and Observed for every lane of the peer, so a lane's sending health is judged from echoes that return on any lane.
- §7.3/§11: status reports bytes sent per link. Ask the owner whether the primary should prefer an unmetered lane (the iPhone hotspot is detectable from 172.20.10.0/28) when that lane carries the offered load.
- §7.3: on metered links, use an idle probe interval just under the measured carrier NAT timeout (start at 25 s) instead of 5 s.
- §12.2: add three acceptance tests. Primary, hotel with a 150 ms delay spike and no loss: no video frame lost. Primary trial switch in both directions: zero frames lost to reordering. Aggregate, video plus saturating bulk: the video's p99 added latency is unchanged by the bulk flow's losses.


## Competitive gaming and game streaming

### Summary

1. Random packet loss is not the problem to solve for Moonlight. By default Sunshine adds 20% Reed-Solomon parity inside each frame. By my binomial calculation for 60 Mbps at 120 fps (74 shards per frame), that loses less than one frame every 8 minutes at 5% independent loss. What breaks the stream is delay spikes, lanes stalling and loss bursts.
2. Each loss that FEC cannot repair costs about one round trip of frozen video, not one frame. Moonlight drops every following frame until a reference-frame-invalidation (RFI) or IDR keyframe arrives. At the owner's ~95 ms hotspot RTT, one bad frame freezes ~12 frames. The A1 target of 99.5% complete frames allows up to 36 lost frames a minute, which is far too lax.
3. Moonlight sends mouse, keyboard and gamepad input on reliable ENet channels. One lost input packet holds that channel for RTT + 4×variance, and the wait doubles on each retry: roughly 130 ms of frozen mouse. Copying input to a second lane costs under 1 Mbps, so wirebond should always do it.
4. In real game streaming, the worst moments come from Wi-Fi delay, not loss. AUGUR (NSDI '24, Tencent Start): median under 30 ms, but 99.9th percentile over 200 ms. About 60% of stalled frames lost no packets, and Wi-Fi and cellular delay spikes were barely correlated. Retransmitting stalled frames over cellular cut stalls 99.5% using 2.7% cellular data. Parity cannot fix delay; only a copy on an independent lane can.
5. Physics limit: reacting to a stall costs at least detection time plus the backup lane's round trip. Stalls shorter than about one RTT (95 ms here) can only be hidden by copying in advance, which costs 100% of the copied flow. Longer stalls can be repaired cheaply by resending recent packets on another lane, about 0.3% extra bandwidth. Today's liveness timer (≥150 ms, ~230 ms at this RTT) only notices a stall after the user has seen it.
6. A copy helps only if the backup lane is independent and lightly loaded. Vulimiri (CoNEXT '13): duplication helps below a 25–50% load threshold. MoMQ: full duplication over Starlink gave about the same 99.9th percentile as Wi-Fi alone (394.5 vs 399.8 ms) at twice the bandwidth. WTFast: copies queued behind the same backlog do not help. The 60 Mbps stream cannot be copied onto the 38 Mbps hotel lane at all.
7. Cross-lane parity (XOR or Reed-Solomon) is never the best choice here. To survive a whole lane stalling it needs at least 100% overhead, which is just duplication. Random loss is already covered by Moonlight's FEC and by games' own input redundancy and interpolation buffers. The FEC that does pay (Tambur, about 26% fewer freezes) works inside the application, where frame boundaries are known; wirebond does not know them.
8. The state of the art is one good path plus cheap copies of small real-time flows. Valve picks one route by ping, with sticky hysteresis, and does not duplicate. WTFast, NoPing, ExitLag and Speedify's Enhance Streaming copy only the small game or real-time flows (WTFast: ~100 kbps per session, doubled). Full copying is a manual mode, capped at the fastest single connection. So wirebond's primary mode plus copied urgent packets matches the field. A genuinely state-of-the-art stability mode adds three things: a duplication budget based on flow rate, resending recent packets when a lane stalls, and stall detection in under 30 ms.
9. Sunshine sends each frame as a burst at about 800 Mbps. A shallow hotel shaper drops packets from these bursts, and the drops cluster enough to defeat FEC. Pacing at the sender to the lane's rate moves the queue somewhere it is lossless and visible, without making frames finish later. L4S trials show bottleneck queues causing 225+ ms spikes, so wirebond should never be the source of a queue.
10. Measure the tail and the freezes, not averages. Xu & Claypool (MMSys '24): how long a playback interruption lasts matters, how often does not. Jitter hurts more than constant delay. Acceptance should use 99.9th-percentile frame delay, the longest frame gap, frames FEC could not repair, and 99.9th-percentile input delay.

### Findings

#### 1. Moonlight already repairs random loss inside each frame. Sunshine adds 20% Reed-Solomon parity to every frame (at least the client's minimum parity count; up to 255 shards per block, up to 4 blocks per frame). Audio uses 4 data + 2 parity shards. So random loss below about 5% almost never costs a frame. A whole frame is lost only when losses cluster or the network stalls.

**Evidence.** Sunshine src/config.cpp: `20, // fecPercentage`. Sunshine src/stream.cpp: fec::encode, parity_shards = ceil(data_shards × pct / 100), and 'Use around 80% of 1Gbps' send pacing (https://github.com/LizardByte/Sunshine/blob/master/src/stream.cpp). moonlight-common-c RtpAudioQueue.h: RTPA_DATA_SHARDS 4, RTPA_FEC_SHARDS 2. My binomial calculation for 60 Mbps at 120 fps (61 data + 13 parity = 74 shards per frame) gives the chance a frame cannot be repaired: 2.4e-10 at 2% loss, 1.6e-5 at 5% (about 0.12 frames a minute), 1.4e-2 at 10% (about 100 frames a minute). A 5 ms stall is 44 packets at about 8,900 packets a second, far more than 13 parity shards can cover.

**Implication.** Add no FEC or parity to wirebond for random loss. Spend wirebond's complexity on what Moonlight cannot repair: lane stalls, delay spikes, loss bursts and sustained overload.

#### 2. Moonlight turns each frame it cannot repair into a freeze of about one round trip. It drops every following frame until an RFI frame or IDR keyframe arrives, and it requests that frame over the control channel.

**Evidence.** moonlight-common-c src/VideoDepacketizer.c: dropFrameState() sets waitingForRefInvalFrame or waitingForIdrFrame, followed by connectionDetectedFrameLoss() or LiRequestIdrFrame(). ControlStream.c sends RFI and IDR requests on CTRL_CHANNEL_URGENT with ENET_PACKET_FLAG_RELIABLE (https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/VideoDepacketizer.c). At the measured ~95 ms hotspot RTT, one lost frame costs about 12 frames at 120 fps. The A1 target of '≥ 99.5% of frames complete' allows 36 lost frames a minute; if they are isolated, that could mean seconds of freeze every minute.

**Implication.** Count 'unrecovered-frame events' and the longest frame gap, not the share of complete frames. Make netsim model Moonlight's freeze of one RTT on each frame that cannot be repaired, or tests will understate the harm by about 10×.

#### 3. Moonlight's input is sent reliably on ENet: mouse, keyboard and each gamepad have their own channel. One lost input packet holds back the rest of that channel until ENet resends it, after RTT + 4 × RTT variance, and the wait doubles on each further loss. Mouse movement is batched every 1 ms, so up to about 1,000 small packets a second.

**Evidence.** moonlight-common-c InputStream.c: enetPacketFlags = ENET_PACKET_FLAG_RELIABLE on CTRL_CHANNEL_MOUSE, KEYBOARD and GAMEPAD; MOUSE_BATCHING_INTERVAL_MS 1. Moonlight's ENet fork, protocol.c: roundTripTimeout = roundTripTime + 4 * roundTripTimeVariance, and roundTripTimeout *= 2 on each timeout (https://github.com/cgutman/enet/blob/master/protocol.c). At 95 ms RTT and ~10 ms variance, that is about 135 ms of frozen mouse, then a catch-up jump. Copying all input costs at most ~0.8 Mbps (1,000 packets/s × ~100 B), and only while the mouse moves.

**Implication.** Keep copying urgent packets in every mode; it is the highest-value redundancy per byte. Moonlight's RFI/IDR requests and ENet acknowledgements are small too, so they should always be copied as well.

#### 4. In game streaming over Wi-Fi, the slowest frames come mostly from delay spikes, not loss, and Wi-Fi and cellular spikes are largely independent. Retransmitting stalled frames on the cellular path, only when needed, removes almost all stalls for a few percent extra data.

**Evidence.** AUGUR (NSDI '24, from Tencent Start cloud gaming), https://www.usenix.org/system/files/nsdi24spring_prepub_zhou-yuhan.pdf. Median RTT and frame delay are under 30 ms, but the 99.9th percentile exceeds 200 ms. About 60% of packets in stalled frames were delivered without loss. The Wi-Fi RTT was already over 200 ms before 65% of stalls on 2.4 GHz. Fluctuations on Wi-Fi and cellular are 'hardly correlated'. A 0.5% rise in stall rate cut retention time by 33%. AUGUR: up to 66% lower tail latency and 99.5% fewer frame stalls, with 2.7% cellular data and 88.1% less cellular data than other multipath schemes.

**Implication.** Parity cannot fix delay; only a copy on an independent lane can. Add AUGUR-style resending on stall: re-send the packets a stalled lane was carrying on another lane. It costs well under 1% on average and fixes stalls longer than about one RTT.

#### 5. Physics sets the boundary between copying in advance and recovering after the fact. A recovered packet arrives no sooner than detection time plus the backup lane's round trip, because the stall must be signalled back and the packet resent. Stalls shorter than about one RTT (~95 ms here) can therefore only be hidden by sending copies in advance. The spec detects a stall far too slowly for gaming.

**Evidence.** SPEC §7.3: a lane is suspect after max(150 ms, 2×RTT + 4×dev) with probes every 50 ms, which is ≥ 230 ms at 95 ms RTT. AUGUR counts a frame over 200 ms as a stall. Community reports, not verified: macOS AWDL takes the radio for ~50–100 ms about once a second (https://gist.github.com/kouwei32/c101be682fc2e433e153ea131798caec), and Wi-Fi spikes run 100–500 ms. Costs at 60 Mbps of video: copying everything costs +60 Mbps (100%). Resending on stall, at 1 event per 10 s with a 30 ms resend window, costs ~225 KB per event, about 0.18 Mbps (0.3%). Copying urgent flows costs ~1–2 Mbps (2–3%).

**Implication.** Detect stalls in under 30 ms, from probes every 10 ms and the per-lane arrival of copied packets. Resend recent packets when a lane stalls, for stalls longer than an RTT. For shorter stalls, copy in advance only the flows that fit the duplication budget. Quote these three costs in the spec.

#### 6. A copy lowers tail latency only when the backup lane is independent and lightly loaded. Copies that load a lane heavily, or that share the bottleneck, cost double and help nothing.

**Evidence.** Vulimiri et al., 'Low Latency via Redundancy' (CoNEXT '13, https://arxiv.org/abs/1306.3707): redundancy improves mean latency only below a load threshold of 25.8–50% (33% for exponential service), and it is worth it above about 16 ms saved per KB. MoMQ (arXiv 2608.10741, https://arxiv.org/html/2608.10741): MPQUIC full duplication over Wi-Fi and Starlink gave 99.9th percentile 394.5 ms vs 399.8 ms on Wi-Fi alone, 'at twice the bandwidth cost'. A selective scheduler got 114 ms using 11.3% of the backup. WTFast's own blog (https://blog.wtfast.com/blog/headend-multi-path-demo): copies do not fix congestion because 'both copies queue behind the same backlog'. Owner's case: 60 Mbps of video cannot be copied onto the 38 Mbps hotel lane.

**Implication.** Copy onto a lane only while that lane stays under about 50% of its capacity. Choose the copy lane for independence first (a different local link, then a different remote link) and lowest one-way delay (OWD) second. The spec's current rule, 'the supplemental lane with the lowest OWD', can pick a lane that shares the primary's link.

#### 7. Deciding 'urgent' by packet size misses real-time flows with bigger packets. A cheap-flow test by data rate is simpler and is the actual economic reason to copy a flow.

**Evidence.** Sunshine audio.cpp: stereo Opus at 96 kbps, or 512 kbps with high-quality audio, in 5 ms packets. At 512 kbps that is ~320 B of Opus per packet, plus the 4+2 parity shards, so it fails SPEC §8.3's '≤ 256 bytes average' rule and is treated as bulk. Game traffic is ~100 kbps per session according to WTFast. Moonlight input is ≤ ~0.8 Mbps. All of these cost under 2% to copy next to a 60 Mbps video stream.

**Implication.** Classify by rate. A flow bucket whose rate over the last 200 ms is under a cheap-flow ceiling (start at 1 Mbps) is copied. Keep the per-packet TCP SYN/FIN/RST/pure-ACK and DSCP EF rule. This covers input, audio, game UDP, DNS, voice and control without a size heuristic.

#### 8. A stability mode at the state of the art is a duplication budget, not a new mechanism. Copy flows from cheapest upward for as long as the copy lane stays lightly loaded. In the owner's hotel, that protects at most about half the second-best lane: ~19 Mbps if hotel is the copy lane, ~40 Mbps if cellular is.

**Evidence.** Speedify (https://support.speedify.com/article/870-bonding-mode): the default 'Enhance Streaming' switches between bonding and redundancy for real-time streams only. Full 'Redundant' mode copies onto up to 3 connections and is capped at the fastest single connection. NoPing and ExitLag market multi-connection copying of game packets (vendor claims, not verified). 3GPP PDCP duplication and IEEE 802.1CB / DetNet PREOF (RFC 8655) likewise turn replication and elimination on per flow according to conditions.

**Implication.** One rule for both modes: duplication budget = cheap flows in the default mode, and everything that fits under 50% of the copy lane in stability mode. `status` reports a 'protected rate'. The README says: for stutter-free play, set Moonlight's bitrate at or below the protected rate, and lower Sunshine's fec_percentage when every packet is copied.

#### 9. Cross-lane parity (XOR or Reed-Solomon spread across lanes) is never the best choice for wirebond. Surviving a whole lane stalling takes parity of at least 100% of that lane's traffic, which is duplication with extra decoding. Random loss is already handled by the applications. The FEC that pays off works at the application layer, where frame boundaries are known.

**Evidence.** Parity at rate r recovers at most a fraction r of a block. A lane that stalls for longer than a block (a frame is 8.3 ms at 120 fps) loses all its shards. Moonlight has in-frame Reed-Solomon (finding 1). Game netcode has its own redundancy: Source's 100 ms interpolation survives one lost snapshot, and extrapolation covers 250 ms (https://developer.valvesoftware.com/wiki/Source_Multiplayer_Networking). Tambur (NSDI '23, https://github.com/Thesys-lab/tambur), an application-layer streaming code: 26% fewer freezes and 35% less redundancy bandwidth than block codes.

**Implication.** Add no parity packet type. Record it in the CONTEXT.md decisions with this reason, so later contributors do not add FEC.

#### 10. Sunshine sends each frame as a burst at about 0.8 Gbps, in batches of up to 64 KB. That is wider than a hotel shaper's bucket, so drops come in bursts that defeat in-frame FEC. Pacing at the sender to the lane's rate gives the same frame completion time without drops, because the queue sits at the sender, where it is visible and lossless, instead of at the shaper.

**Evidence.** Sunshine stream.cpp: ratecontrol_packets_in_1ms = 1 Gbps × 80% (packets per 1 ms), and send_batch_size = min(64, 64 KiB / blocksize). At 60 Mbps and 120 fps a frame is ~62 KB, sent in ~0.8 ms. The spec's hotel shaper has a 64 KB burst on a 38 Mbps rate. L4S trials: GeForce NOW lag spikes of 225+ ms fell to ~20 ms (SamKnows, as reported by Comcast); NVIDIA reported an ~85% drop in 99th-percentile RTT (vendor figures, https://datatracker.ietf.org/meeting/119/materials/slides-119-maprg-l4snqb-field-trial-measurements-livingood-00).

**Implication.** Use aggregate mode's per-lane pacer in primary mode too, at about 1.0–1.1 × learned capacity once capacity is known, and unpaced while it is unknown. This is one mechanism for both modes, and it turns shaper tail-drop into a sender queue whose delay wirebond can measure.

#### 11. Valve and Riot build on one good path with hysteresis plus redundancy inside the game, not multipath duplication. Duplication products target access-network loss, and only for small flows.

**Evidence.** Valve GameNetworkingSockets, steamnetworkingsockets_p2p.cpp: routes are scored by ping plus penalties, one transport is selected, and a 'sticky' penalty means a new route must beat the current one on score×1.1+5 and on its worst-case score. No packet duplication (https://github.com/ValveSoftware/GameNetworkingSockets). Riot Direct is a private backbone that fixes routing detours, e.g. 14 ms vs 70 ms SF–Portland (https://technology.riotgames.com/node/25). WTFast's CS2 emulator test (vendor, not independent): with 10% injected loss, dropped player inputs fell from 9.6% to 0.0% and on-time ticks rose from 90.1% to 99.5%, at about double the game bandwidth (~100 to ~200 kbps).

**Implication.** Primary mode plus copied urgent flows is the right default (it validates R7). Borrow Valve's sticky hysteresis for promotion so A3 (no flapping) holds by construction.

#### 12. Players are hurt most by how long an interruption lasts and by jitter. Constant latency degrades performance linearly; how often small hitches happen matters little.

**Evidence.** Xu & Claypool, MMSys '24 (https://web.cs.wpi.edu/~claypool/papers/frame-variation-mmsys-24/paper.pdf): from four user studies, 'playout interruption sizes matter while interrupt frequencies do not'. A WPI 2025–26 FPS study (25 participants, delay 0–100 ms, jitter 0–120 ms): delay had minimal effect, jitter noticeably lowered quality of experience (https://web.cs.wpi.edu/~claypool/iqp/delay-jitter-cloud-26/). Claypool & Finkel, NetGames '14: quality and performance in cloud games fall linearly with latency, much like first-person games.

**Implication.** Write acceptance criteria as 99.9th-percentile frame delay, longest frame gap per minute, unrecovered-frame events, and 99.9th-percentile one-way delay for input packets. Accept a few ms of median delay in exchange for removing 100+ ms interruptions.

#### 13. The packets wirebond already copies give free, fine-grained lane telemetry. Both copies carry the same counter, so the receiver sees the exact one-way-delay difference between lanes, and notices when a lane falls silent, as often as input flows (up to 1,000 Hz) with no clock synchronization. Faster probes are cheap when the link is idle.

**Evidence.** SPEC §8.1: one counter per direction shared across lanes. §8.2: the replay window already sees both copies. A probe is 75 B of payload + 54 B overhead = 129 B. At 100 Hz that is ~0.1 Mbps per lane per direction, compared with 50 ms cadence and a ≥150 ms suspect floor today.

**Implication.** The receiver records, per lane, the arrival lag of copied packets. If lane A delivers no copies of packets that lane B has delivered for more than max(10 ms, 3 × jitter), the receiver reports right away on lane B. Probe every 10 ms while data flows. Expected result: stalls detected in ~10–30 ms instead of 150–230 ms.

#### 14. Copies over a phone hotspot use metered data, and wirebond users cannot see it. Most users care about this.

**Evidence.** AUGUR survey of 1,251,420 users: 57.0% unwilling to spend cellular data on streaming; 62.9% of the rest want to reduce it. Arithmetic: copying 1 Mbps costs 0.45 GB/h; copying 20 Mbps of video costs 9 GB/h. T-Mobile hotspot data is capped (plan-dependent, not checked).

**Implication.** `status` shows the bytes each lane has carried, copies included. Stability mode states its data cost in GB/h. Copying only cheap flows by default keeps metered use under about 0.5 GB/h.

### Proposals

- SPEC §8.3: replace the '≤ 256-byte average' urgent rule with a rate rule: a flow bucket is urgent while its rate over the last 200 ms is under a named constant CheapFlowRate = 1 Mbps; keep the per-packet TCP SYN/FIN/RST/pure-ACK and DSCP EF/CS6 rules.
- SPEC §9.1 step 1: choose the copy lane by independence first (a different local link, then a different remote link from the primary) and lowest OWD second, and copy onto it only while its send rate plus the copies stays under 0.5 × its capacity, or its lower bound when capacity is unknown.
- SPEC §9: define one duplication budget used by both behaviours: by default it covers cheap flows, and `wirebond mode stable` extends it, cheapest flow first, to every flow that fits under 0.5 × the copy lane's capacity; `status` prints the resulting 'protected rate' in Mbps and the extra data cost in GB/h.
- SPEC §9.1: add stall resending: each lane keeps a ring buffer of the sealed packets it sent in the last max(RTT, 100 ms); when a lane turns suspect, every packet sent on it after the last counter the peer confirmed is resent once on the copy lane, and the existing replay window drops the losing copy.
- SPEC §7.3 Probe: add LastCounter (u64), the highest data counter received on this lane, so the sender knows exactly which packets to resend when the lane stalls.
- SPEC §7.3: while data flows, probe every 10 ms instead of 50 ms, and lower the suspect floor from 150 ms to max(30 ms, 3 × probe interval, 2 × OWD jitter on that lane), at a cost of about 0.1 Mbps per lane per direction.
- SPEC §7.3: add detection at the receiver: when copies of packets keep arriving on lane B but not on lane A for more than max(10 ms, 3 × jitter), the receiver sends a probe on B at once marking A as lagging, and the sender treats A as suspect.
- SPEC §9.1: pace the primary lane with the same per-lane FIFO pacer as aggregate mode, at 1.0–1.1 × learned capacity once capacity is known, so Sunshine's 0.8 Gbps bursts per frame queue at the sender instead of tail-dropping at a hotel shaper.
- SPEC §9.1 step 5: make promotion sticky like Valve's: a candidate must beat the primary by 10% plus 5 ms (or plus 10% delivered rate), held for 3 s, so A3 holds by construction.
- CONTEXT.md decisions: add 'No FEC or cross-lane parity in wirebond', because Sunshine's 20% in-frame Reed-Solomon and games' own redundancy already handle random loss, and surviving a whole lane stalling needs at least 100% parity, which is duplication.
- SPEC §12.1 netsim: add a Moonlight model (20% in-frame Reed-Solomon, a freeze of one RTT on each frame it cannot repair, input on reliable ENet resent after RTT + 4 × variance with doubling backoff), Sunshine's 64 KB bursts at 0.8 Gbps, and a Wi-Fi radio-pause process of 50–100 ms pauses about once a second on the hotel lane.
- SPEC §12.2 A1: replace '≥ 99.5% of frames complete, p99' with '≤ 1 unrecovered-frame event per 5 minutes, 99.9th-percentile frame delay ≤ cell OWD + 20 ms, and no frame gap over 100 ms outside injected failures'; add A9: with the hotel lane pausing 80 ms once a second, no copied input packet is delayed more than 5 ms beyond the cell lane's OWD.
- SPEC §11 status: show bytes carried per lane, copies included, and the protected rate, so the cost of copies on a metered phone hotspot is visible.
- README: tell Moonlight users to set the stream bitrate at or below wirebond's protected rate for stutter-free play, and say that with everything copied, Sunshine's fec_percentage can be lowered from 20.


## Mesh overlays and trust

### Summary

1. wirebond can drop most of its own security code. Use wireguard-go's `device` package and give it a custom `conn.Bind` that owns the lanes. That package comes from `golang.zx2c4.com/wireguard`, which SPEC §4.2 already depends on for `tun`, so no new module is needed. It replaces the TLS control channel's key export (§6.4), the per-connection key generations, the ChaCha20 sealing (§8.1), the replay window (§8.2), the handshake, rekeying and DoS cookies with an audited protocol: WireGuard's Linux version is under 4,000 lines and the protocol has had formal analysis. Tailscale's magicsock shows this Bind pattern works in production.
2. Two adaptations make it work for wirebond. First, give each peer a virtual endpoint and turn off WireGuard's roaming with `DisableSomeRoamingForBrokenMobileSemantics()`, so only wirebond's authenticated probes move lane addresses. Second, send WireGuard handshake packets (types 1–3) on every lane, so a rekey never waits on one lossy lane.
3. Costs. The Bind sees only ciphertext, so classification reduces to the WireGuard message type and the padded length. DSCP and per-flow buckets go away unless wirebond forks wireguard-go. Per-packet overhead rises from 54 to 60 bytes over IPv4.
4. Gains beyond less code. The same ciphertext sent on several lanes is deduplicated for free by wireguard-go's 8,128-counter replay window, which matches the spec's 8,192 sizing. The WireGuard counter is visible in the clear, so aggregate mode can reorder by (receiver index, counter) and needs no 0x04 type or `oseq` field.
5. Keys should be agreed over the lanes, not over Tailscale TCP. Tailscale keeps one best path, picked by latency. It re-checks paths every 3 s (heartbeat), trusts a path for 6.5 s and looks for a better one every minute, so it is a slow and fragile place for keying. WireGuard rekeys every 120 s, which gives forward secrecy; the spec's TLS-exported keys change only when the connection reconnects.
6. Trust model: for one owner with fewer than 20 devices and no server, the simplest sound choice is a membership set of WireGuard public keys that any member can add to or remove from, with removals permanent. It needs no CA, X.509, CSR or signatures. In the spec's CA model, losing the one device that holds `network.key` means no more invites; Tailscale's Tailnet Lock requires at least 2 signers for exactly this reason. A shared network secret is rejected because revoking it means rotating it on every device.
7. Invites can follow innernet's model. The token carries a one-time secret from which both sides derive a temporary WireGuard key and a temporary overlay address. The joiner connects as that temporary peer and registers its real key inside the tunnel, so there is no TLS, no PAKE and no CSR.
8. Relays through other devices are cheap to add as one more kind of lane. Tailscale's peer relays and Nebula's relays both forward end-to-end-encrypted WireGuard or Noise packets without decrypting them; Tailscale reports relay throughput close to a direct connection. The RON study found detour paths around 60–100% of major outages, cut latency by 40 ms in about 11% of samples and doubled TCP throughput in about 5%.
9. Phones, in increasing order of effort:
   - (a) The iPhone hotspot over USB is already a lane, with no code.
   - (b) If the wire format is WireGuard, the official WireGuard app can join as a single-path member from a QR code wirebond prints.
   - (c) Full multipath members later: the same Go data plane inside an iOS Network Extension (50 MiB memory limit, which wireguard-go already has iOS-specific settings for) or an Android VpnService that binds a socket to cellular.
10. None of Nebula (about 42.7k lines, needs a lighthouse), innernet (needs a server), Yggdrasil (open network, addresses derived from keys, no authorization) or ZeroTier (needs a controller; multipath is set up by hand in each node's `local.conf`) fits better. ZeroTier's active-backup and balance-aware modes are the closest prior art to wirebond's primary and aggregate modes.

### Findings

#### 1. The WireGuard protocol already provides every security property the spec builds by hand (§6.4, §8.1, §8.2): a 1-RTT mutually authenticated handshake, transport sealing with a nonce counter, a replay window, timed rekeying, staying silent to unauthenticated senders, and a cookie DoS defence. The wireguard-go `device` package implements all of it.

**Evidence.** WireGuard whitepaper (https://www.wireguard.com/papers/wireguard.pdf): the Linux implementation is under 4,000 lines; Noise IK handshake; the cookie secret changes every 2 minutes, and cookie replies are sent only under load and bound to the initiation's mac1.

wireguard-go at commit b3bc704e (2026-10-25), https://github.com/WireGuard/wireguard-go:
- device/noise-protocol.go: initiation 148 B, response 92 B, cookie reply 64 B, transport header 16 B plus a 16 B tag.
- device/constants.go: RekeyAfterTime 120 s, RejectAfterTime 180 s, RekeyTimeout 5 s, KeepaliveTimeout 10 s.
- replay/replay.go: window = (128−1)×64 = 8,128 counters.

Formal verification (Tamarin, CryptoVerif) is listed at https://www.wireguard.com/formal-verification/ (page not fetched in this run).

**Implication.** Adopt. wirebond should not invent its own AEAD and key-generation scheme; V1's review already found a nonce-reuse bug in exactly that kind of code.

#### 2. wirebond can embed wireguard-go's device and replace only the transport, through the `conn.Bind` interface: Open, Close, SetMark, Send(bufs, ep), ParseEndpoint, BatchSize. Tailscale's magicsock is a production Bind that does path selection this way. The module is already a spec dependency.

**Evidence.** conn/conn.go Bind and Endpoint interfaces (wireguard-go, as above).

Non-test line counts measured at that commit: device 4,993; conn 2,321; tun 4,557 (already used by SPEC §4.2); replay 62; ratelimiter 137.

Tailscale wgengine/magicsock: magicsock.go 4,771 lines plus endpoint.go 2,333 lines (https://github.com/tailscale/tailscale/tree/main/wgengine/magicsock). This is a warning that a Bind can grow very large if it takes on discovery, DERP and relays.

UNVERIFIED ESTIMATE (no wirebond code exists yet): about 1,000–1,500 lines of wirebond not written (internal/identity X.509 and CSR, the TLS control channel with generations and key export, internal/wire sealing and replay), offset by about 300–500 lines of Bind.

**Implication.** Adopt. internal/wire becomes a Bind in internal/lane. internal/identity shrinks to one X25519 key. §4.2's module list stays the same; x/crypto is needed only for probe sealing.

#### 3. WireGuard roaming conflicts with the spec's security rule. WireGuard moves a peer's endpoint on any authenticated packet; the spec moves lane addresses only on a fresh probe. wireguard-go can be stopped from roaming, so the Bind alone chooses lanes.

**Evidence.** Whitepaper §2.1: the endpoint is learned from the outer source address of a correctly authenticated packet.

wireguard-go:
- device/peer.go: SetEndpointFromPacket returns early when disableRoaming is set.
- device/mobilequirks.go: DisableSomeRoamingForBrokenMobileSemantics() sets disableRoaming for every peer that has an endpoint.
- device/uapi.go: `endpoint=` is parsed by bind.ParseEndpoint, so a custom string such as `peer:10.77.0.2` works.
- Cookie replies are sent to the received packet's own endpoint, so the Bind must also accept plain address endpoints.

**Implication.** Configure every peer with a virtual endpoint that names the peer, and call DisableSomeRoamingForBrokenMobileSemantics(). The Bind maps the peer to its lanes and keeps §7.3's rule that only probes move addresses.

#### 4. Behind a Bind, wirebond sees only ciphertext. Per-packet urgency must come from the WireGuard message type and length (plaintext is padded to 16 bytes). DSCP and per-flow buckets (§8.3) are not visible without forking wireguard-go.

**Evidence.** device/send.go: the padded size is rounded up to a multiple of PaddingMultiple (16). Ciphertext is 32 + roundup16(IP length), so a 40-byte TCP ACK becomes 80 B and a 1,068-byte Moonlight video packet becomes 1,104 B.

Packets are staged and nonces assigned in send order (send.go:356). A side channel from a tun wrapper would break whenever the staging queue drops packets.

Tailscale maintains its own fork (github.com/tailscale/wireguard-go) for hooks like this.

**Implication.** Simplify §8.3. Urgent means a WireGuard handshake message, or a transport packet of 288 B or less (that is, at most 256 B of IP). The per-flow 200 ms average and DSCP go away. Accept that small packets of a mixed flow can overtake its large ones by the OWD gap between lanes (about 15 ms in the hotel case); TCP's RACK tolerates that. Do not fork.

#### 5. Duplicating a packet across lanes and putting packets back in order need no new fields when the wire format is WireGuard. Identical ciphertext on two lanes is dropped by the replay window, and the counter needed for reordering is in the clear.

**Evidence.** Replay window of 8,128 counters (replay/replay.go). The spec sizes 8,192 for 50 ms of skew at 160k packets/s, so the two are equivalent.

The transport header carries a 4-byte receiver index and an 8-byte little-endian counter at bytes 8–15 (device/send.go:457).

WireGuard rekeys every 120 s, and each new keypair restarts its counter at 0, so reordering must be keyed by (receiver index, counter).

**Implication.** For V3, drop packet type 0x04 and `oseq`. The receive side of the Bind holds transport packets for at most min(50 ms, skew + 2 × jitter) and releases them in counter order. Small (urgent) packets skip the buffer, but their counters count as seen. An off-path attacker would need the random 32-bit receiver index to disturb the buffer.

#### 6. The spec runs key agreement over TCP and TLS through Tailscale. That makes Tailscale's slow path recovery a single point of failure for keys, and the keys rotate only when the TLS connection reconnects.

**Evidence.** tailscale wgengine/magicsock/magicsock.go about lines 4167–4209: heartbeatInterval 3 s, trustUDPAddrDuration 6.5 s, upgradeUDPDirectInterval 1 min, sessionActiveTimeout 45 s.

endpoint.go betterAddr: one best address chosen by a latency score; relay paths (Geneve) rank below direct ones.

Tailscale has no multipath data path; this search found nothing.

SPEC §6.4: 'There is no timed rekey.'

**Implication.** Agree keys over the lanes themselves. WireGuard handshakes travel on every lane, including the Tailscale fallback lane. Then losing Tailscale, or its path, never blocks keying, and keys rotate every 120 s. Add an acceptance test: rekey succeeds while the primary lane has 30% loss or is down.

#### 7. Trust models for one owner with fewer than 20 devices and no server: the CA model (current spec, Nebula) concentrates the power to invite on one device; Tailnet Lock needs 2 or more signers; a shared secret cannot be revoked well. The simplest sound option is an any-member membership set of WireGuard keys with permanent removals.

**Evidence.** SPEC §6.1: only the device holding network.key can invite or revoke, so losing it means re-initialising the network.

Tailnet Lock (https://tailscale.com/kb/1226/tailnet-lock): at least 2 signing nodes, at most 20. Removing a signer re-signs the nodes it signed by default. Ten disablement secrets are printed once.

Nebula's cert package is 2,849 non-test lines (measured, https://github.com/slackhq/nebula/tree/master/cert).

Shared secret: revoking it means rotating it on every device, devices that are offline are stranded, and a leaked secret grants permanent access.

In an any-member model a member's claim that N is a member carries the same authority as an invite from that member, so records need no signatures. Gossip runs over WireGuard-authenticated sessions, and forged endpoints simply fail the probes.

**Implication.** Simplify §6.1 and §6.3. state = device.key (X25519) + members.json + config.json. A member record is (public key, name, overlay address, added by); merging takes the union of adds, removals win, and a removed key is never re-added. `remove` lists the devices the removed one added and offers to remove them too. Drop network.crt, network.key, device.crt, revocation serials and Ed25519.

#### 8. An invite can be a temporary WireGuard identity, as in innernet. This removes the unauthenticated TLS join listener (§6.2, `VerifyClientCertIfGiven`).

**Evidence.** innernet (https://github.com/tonarino/innernet): the invitation is single-use, and the client generates its own key pair, after which the invitation's private key becomes unusable. innernet needs a coordination server, which wirebond avoids.

In wirebond both sides derive the temporary key from the token secret (HKDF to an X25519 private key). The inviter installs that temporary peer with a temporary /32 for 24 h. The joiner sends its real public key over the tunnel and receives the member list.

**Implication.** Token = `wb1:` + inviter public key + inviter lane and control addresses + 32-byte secret + expiry. The join listener then accepts only WireGuard packets, so nothing unauthenticated runs on any port.

#### 9. How mesh overlays relay through another node: Tailscale peer relays forward opaque WireGuard packets between two bound addresses. They use a Geneve header (24-bit VNI), a 3-way bind handshake with a BLAKE2s MAC challenge, a 30 s bind lifetime and a 5 min idle lifetime, and Tailscale prefers direct > peer relay > DERP. Nebula relays add a per-hop AEAD tag over the end-to-end ciphertext and must be configured explicitly.

**Evidence.** https://github.com/tailscale/tailscale/blob/main/net/udprelay/server.go: defaultBindLifetime 30 s, defaultSteadyStateLifetime 5 min, MAC secret rotated every 2 min, 7 MiB socket buffers. The relay holds only disco keys and returns bytes unmodified.

https://tailscale.com/blog/peer-relays-beta (2025-10-29): throughput 'nearing that of a direct connection'; one UDP port reachable from both sides; enabled with `tailscale set --relay-server-port`; client version 1.86 or later.

Nebula outside.go handleOutsideRelayPacket and connection_state.go VerifyRelay ('The entire body is sent as AD'); examples/config.yml has `am_relay` and `relays`.

**Implication.** Adopt a minimal relay lane. Prefix the WireGuard packet with a 4-byte header (relay type + destination host). A relay forwards only from addresses that passed a member's probe in the last few seconds, and only to a member's current lane, so it is never an open reflector. End-to-end WireGuard protects the payload. Probes run end-to-end through the relay, so the scheduler treats a relay lane like any other lane with higher OWD. Every member relays by default; relays need no grants because every member is trusted.

#### 10. Detour routing through a third node has measured value beyond NAT traversal: it routes around outages and sometimes beats the direct path on loss, latency and throughput.

**Evidence.** Resilient Overlay Networks (Andersen et al., SOSP 2001, https://www.cs.princeton.edu/courses/archive/fall06/cos561/papers/ron.pdf): routed around 60–100% of significant outages, usually in under 20 s; cut latency by 40 ms in about 11% of samples; at least 0.05 lower loss in about 5%; doubled TCP throughput in about 5%. These are 2001 measurements on 12–13 nodes and are only indicative today.

**Implication.** Measure, don't assume. Relay lanes through the home Pi or Xanth enter the normal lane set; the primary-mode scheduler promotes one only when it actually carries the traffic better. Add an acceptance test: the direct lane is blackholed and a relay lane takes over within the A2 bounds.

#### 11. Phones can take part in three increasing steps. A WireGuard wire format gives a zero-code first step.

**Evidence.** (a) The iPhone USB hotspot is already a lane (CONTEXT.md).

(b) The official WireGuard apps run wireguard-go: wireguard-go has iOS-specific constants (device/queueconstants_ios.go: MaxSegmentSize 1700, QueueStagedSize 128).

(c) A native member app:
- iOS: the NEPacketTunnelProvider memory limit is 50 MiB on iOS 15–26 per Apple DTS (https://developer.apple.com/forums/thread/73148?page=2).
- Android: VpnService.setUnderlyingNetworks plus Network.bindSocket allows binding to cellular (https://developer.android.com/reference/android/net/VpnService). Reports of carrier and Pixel quirks are anecdotal.
- Wi-Fi and cellular at the same time on iPhone is shown by Speedify's App Store app (https://support.speedify.com/article/960-combine-wifi-cellular-iphone, vendor source).

Tailscale's 2026 post: wireguard-go reads each packet into a 64 KiB buffer (https://tailscale.com/blog/making-tailscale-faster), which matters under 50 MiB.

**Implication.** V2: `wirebond invite --wireguard` prints a wg-quick config and a QR code. The phone becomes a single-path, leaf member that cannot invite; it uses a member with a reachable endpoint as its WireGuard peer. Later: a gomobile build of the same Go data plane, which can also act as a relay lane for a laptop on the same Wi-Fi. Set GOMEMLIMIT about 40 MiB on iOS and shrink the buffers.

#### 12. wireguard-go's throughput is far above wirebond's needs, but a custom Bind gives up wireguard-go's built-in batching and offloads (sendmmsg, GSO, GRO) unless wirebond reimplements them.

**Evidence.** https://tailscale.com/blog/more-throughput (2023-04-13, v1.40): over 10 Gb/s on bare-metal Linux with UDP GSO/GRO and checksum work.

The largest targets in the spec: 118 Mbps aggregate in the hotel, a 1 Gbps home line.

WireGuard overhead: 32 B + 8 UDP + 20 IPv4 = 60 B, against the spec's 54 B. With MTU 1,280, outer packets are 1,340 B (IPv4) or 1,360 B (IPv6), which still fits every real path.

**Implication.** Use simple one-packet-per-call per-link sockets on all OSes for V2, with BatchSize 1 or a small batch. Add Linux sendmmsg or GSO only if a Pi measurement demands it. Update §7.5's overhead numbers to 60 B and 80 B.

#### 13. ZeroTier is the only mainstream mesh overlay with built-in multipath, and it needs hand configuration. Nebula, innernet and Yggdrasil each fail at least one of: no server, small size, authorization.

**Evidence.** ZeroTier (https://docs.zerotier.com/multipath/): active-backup, broadcast, balance-rr (packetsPerLink 64), balance-xor, and balance-aware with lat_weight/pdv_weight link-quality hints. It is configured per node in local.conf; example failover intervals are 500–5,000 ms.

Nebula: 42,659 non-test Go lines (measured) and needs a lighthouse.

innernet: needs innernet-server.

Yggdrasil (https://yggdrasil-network.github.io/2018/07/28/addressing.html): address = SHA-512 of the public key truncated into 200::/7. Anyone may join; there is no authorization.

Key-derived IPv4 addresses inside a /16 would collide with probability about 20×19/2/65,534 ≈ 0.29% for 20 devices (computed).

**Implication.** Keep the automatic primary and aggregate modes; ZeroTier's manual policies are what wirebond's automatic modes should beat. Keep 'lowest free address' assigned by the inviter. On a gossip conflict (concurrent invites during a partition), the lower public key keeps the address and the other device re-picks.

### Proposals

- In §4.2 and §8, replace the custom sealing, replay window and TLS-exported key generations with wireguard-go's `device` package driven by a wirebond `conn.Bind` in internal/lane, and delete internal/wire.
- In §6.4, remove TLS 1.3, X.509 and the hello/generation exchange; peers exchange records and member lists in small JSON messages to each other's overlay address on port 7849, inside WireGuard.
- In §6.1, replace the network CA with members.json: a set of (WireGuard public key, name, overlay address, added by) that any member can add to or remove from, where removals are permanent and win over adds.
- In §6.2, make the invite token carry a one-time secret from which both sides derive a temporary WireGuard key pair and a temporary /32, valid once for 24 h, as innernet does.
- In §5.5, reduce the state directory to config.json, device.key (X25519) and members.json (with cached lane addresses).
- Configure each wireguard-go peer with a virtual endpoint `peer:<overlay>` parsed by the Bind, and call `DisableSomeRoamingForBrokenMobileSemantics()`, so only authenticated probes move lane addresses (§7.3 unchanged).
- Send every WireGuard handshake message (types 1–3) on all up lanes, and add an acceptance test that rekeying succeeds while the primary lane has 30% loss.
- Seal probes outside WireGuard with XChaCha20-Poly1305: the key comes from X25519(static, static) through HKDF, the nonce is random, the first byte lies outside 1–4, and each probe carries a boot epoch with Seq so a restart cannot replay addresses.
- In §8.3, classify by WireGuard type and ciphertext length only (handshakes, and transport packets of at most 288 bytes, are urgent), and drop DSCP and the per-flow buckets.
- In §9.2, drop type 0x04 and `oseq`; the receiving Bind reorders transport packets by (receiver index, little-endian counter) and lets small packets bypass the buffer.
- Add relay lanes in §7: any member with a reachable lane port forwards opaque WireGuard packets behind a 4-byte header, only between addresses that recently passed a member's probe, and the scheduler treats them as ordinary lanes.
- Move phones out of §15 into a V2 step: `wirebond invite --wireguard` prints a wg-quick config and QR code, so the official WireGuard app can join as a single-path member that cannot invite.
- Update §7.5's overhead figures to WireGuard's 60 bytes (IPv4) and 80 bytes (IPv6), giving outer packets of at most 1,340 and 1,360 bytes at MTU 1,280.
- Add acceptance test A9: with the direct lane blackholed in both directions, a relay lane through a third device carries the stream within A2's bounds.


## Phones on the mesh

### Summary

Phones can become wirebond devices, but only as devices that run while in use, under platform rules the current spec assumes away. The decision-relevant conclusions:

1. Each phone allows only one VPN at a time, on both iOS and Android, so while wirebond runs on a phone, Tailscale cannot. A phone therefore cannot use Tailscale as its control path, and the control channel needs an address it can reach without Tailscale. The cheapest fix is a TCP forward of 7849 next to the existing UDP forward.
2. An iOS packet tunnel stops when the phone sleeps. An Apple networking engineer said it cannot be kept running while the phone is locked ("100%, no"), and a 2026 developer reports system sleep about 10 s after the screen turns off. That rules out a phone that relays a laptop's traffic or stays reachable all the time. The laptop's cellular lane should stay USB Personal Hotspot.
3. Android is the easy platform. requestNetwork(TRANSPORT_CELLULAR) is documented to bring cellular up while another network is the default, and Network.bindSocket(fd) pins a lane socket to it. Go's net.Interfaces() fails on Android 11 and later (golang/go#40569, still open), so the Android app must supply the list of links.
4. On iOS, using Wi-Fi and cellular at the same time from a packet tunnel is contested. In 2017–2021, binding a socket to cellular gave EADDRNOTAVAIL, and Apple DTS advised against binding. Speedify ships Wi-Fi plus cellular bonding on iOS (its own claim), and a 2026 developer relays over sockets on both interfaces. Prove it on the owner's T-Mobile iPhone in a one-day spike before committing.
5. The iOS memory limit is about 50 MiB (iOS 15 to 26; it was 15 MB before). That fits if every buffer has a byte cap and Go's memory limit is set. WireGuard and Tailscale show the patterns: smaller queues, more aggressive GC, GOMAXPROCS=1.
6. The spec's idle probe (every lane, every 5 s) would keep a phone's cellular radio in its high-power state all the time: LTE stays in that state for about 11.6 s after each packet, at about 1 W. Cellular carrier NATs time out idle UDP after a median of 65 s. Idle phones and metered links need one keepalive every 25 s or so and dormant lanes.
7. T-Mobile is IPv6-only with 464XLAT, so the phone's cellular IPv4 address is a 192.0.0.x CLAT address that no peer can reach. Publish IPv6 candidates and filter 192.0.0.0/29.
8. Multipath TCP and Wi-Fi Assist do not help: they work only for TCP, and MPTCP aggregate mode is for experimentation on developer devices only.
9. Leanest path: change no data-plane logic now, but add five seams to the core (links from the host, a hook that binds lane sockets, the overlay interface (TUN) as a file descriptor, sleep and wake events, byte-capped buffers). Later, add one small mobile Go package built with cgo (gomobile bind for Android, as Tailscale does; an iOS xcframework), with a thin Kotlin or Swift shell whose UI is a pasted invite and an on/off toggle. Android first, iOS after the spike.
10. iOS cannot meet R2, R3 or R4 as written: App Review guideline 2.5.2 forbids downloading code, so there is no self-update, and TestFlight builds expire after 90 days. Phones need their own requirement rows.

### Findings

#### 1. Only one VPN-type tunnel can run at a time on iOS and on Android, so a phone running wirebond cannot also run Tailscale, and the spec's control path (Tailscale addresses, §6.5) does not exist on a phone.

**Evidence.** Android VpnService reference: "There can be only one VPN connection running at the same time. The existing interface is deactivated when a new one is created." (https://developer.android.com/reference/android/net/VpnService). Apple: only one enterprise VPN configuration can be enabled at a time, and packet tunnel providers count as enterprise VPNs (https://developer.apple.com/documentation/networkextension/netunnelprovidermanager, https://developer.apple.com/forums/thread/675507). Tailscale's docs confirm another VPN disables Tailscale On Demand on iOS (https://tailscale.com/kb/1291/ios-vpn-on-demand).

**Implication.** Adopt a control path that works without Tailscale. The leanest option: the home device's `endpoint` also serves the control channel on TCP 7849 (the owner forwards TCP and UDP 7849), and LAN addresses keep working. Join can still happen over Tailscale before wirebond starts, but steady-state control must not need it. Avoid embedding tsnet on phones: it is heavy against a 50 MiB limit.

#### 2. An iOS packet tunnel does not run while the phone sleeps, which comes about 10 s after the screen turns off, so a phone cannot be an always-on peer or relay.

**Evidence.** Apple networking engineer Matt Eaton (June 2024), asked whether a Wi-Fi-to-cellular relay tunnel can be kept running while the phone is locked: "100%, no". He points to the provider's sleep and wake methods instead (https://developer.apple.com/forums/thread/756941). A June 2026 developer reports system sleep "after about 10 seconds" with the screen off and "the network extension simply won't run any code"; Apple did not dispute it (https://developer.apple.com/forums/thread/832009). WireGuard patches Go's runtime to use mach_continuous_time so timers expire correctly after sleep (https://raw.githubusercontent.com/WireGuard/wireguard-apple/master/Sources/WireGuardKitGo/goruntime-boottime-over-monotonic.diff, golang/go#24595).

**Implication.** Treat phones as devices that run while in use, which matches the main use: Moonlight on the phone with the screen on. The core needs a Sleep/Wake event: on wake, treat all links as changed, probe at 200 ms, and drop RTT and OWD samples that span the gap. Detecting a run-loop gap of more than 1 s handles this without patching the Go runtime.

#### 3. Using Wi-Fi and cellular at the same time from inside an iOS packet tunnel is possible but contested and fragile, so it is the one real technical risk for iOS.

**Evidence.** Against: in 2017–2021, bind() or IP_BOUND_IF to cellular inside a packet tunnel succeeded, but connect() then failed with EADDRNOTAVAIL after the startTunnel completion handler ran. Apple DTS (Matt Eaton, Oct 2021) advised "stay away from trying to bind a BSD socket to a network interface from this context" (https://developer.apple.com/forums/thread/692930, https://developer.apple.com/forums/thread/76711). In 2018 Quinn called networking in packet tunnel providers "a restricted networking environment". For: Quinn's general guidance names IP_BOUND_IF and NWParameters.requiredInterface as the ways to pin to an interface (https://developer.apple.com/forums/thread/734359). Speedify ships Wi-Fi plus cellular bonding on iOS (vendor claim, https://speedify.com/blog/combining-internet-connections/combine-connections-on-ios/). A 2026 developer describes "sockets on the WiFi and cellular" relaying inside an NE (thread 832009). Unverified: whether unconnected sendto() on IP_BOUND_IF sockets, which is what wirebond's lanes use, avoids the EADDRNOTAVAIL path.

**Implication.** Measure before building iOS: a one-day spike with a packet tunnel on the owner's T-Mobile iPhone that sends UDP to Xanth over Wi-Fi and cellular at the same time, tried both with BSD sockets plus IP_BOUND_IF and with NWConnection plus requiredInterface. The core must let the platform create or bind lane sockets instead of always binding them itself.

#### 4. Android officially supports keeping cellular up while on Wi-Fi and pinning sockets to it, so Android is the low-risk first phone.

**Evidence.** ConnectivityManager.requestNetwork: "an application could use this method to obtain a connected cellular network even if the device currently has a data connection over Ethernet. This may cause the cellular radio to consume additional power." It needs CHANGE_NETWORK_STATE. Unregistering the callback disconnects networks "brought up only to satisfy that request". Each app may have at most 100 outstanding requests (https://developer.android.com/reference/android/net/ConnectivityManager). Network.bindSocket(FileDescriptor) has existed since API 23; the socket "must not be connected" (https://developer.android.com/reference/android/net/Network). VpnService.protect(fd) keeps the app's own sockets out of the tunnel. Unverified: Doze behaviour for VPN apps; sources conflict.

**Implication.** Adopt: Android shell holds one cellular request and one Wi-Fi request while the tunnel is up, passes each Network's handle to Go, and Go calls back to bind each lane fd (bindSocket plus protect). Release the cellular request when wirebond is off. Measure battery and Doze on a real device.

#### 5. Go's net.Interfaces() fails on Android 11 and later (apps targeting API 30+), so wirebond's link discovery (§7.1) cannot run as written on Android.

**Evidence.** golang/go#40569 is still open ("x/mobile: Calling net.InterfaceAddrs() fails on Android SDK 30"), because Android 11 forbids bind() on NETLINK_ROUTE sockets (https://github.com/golang/go/issues/40569). Tailscale registers a Java-backed interface getter with netmon (https://github.com/tailscale/tailscale-android/pull/21). wlynxg/anet patches around it with a //go:linkname hack that needs extra linker flags on Go 1.23+ (https://github.com/wlynxg/anet).

**Implication.** Make link discovery injectable: the daemon uses net.Interfaces() on desktops, and a phone host supplies links (name, index or network handle, addresses, metered flag) from Android's NetworkCallback or iOS's NWPathMonitor. Avoid anet's linkname hack.

#### 6. On phones the overlay interface arrives as a file descriptor created by the OS, and the OS, not wirebond, sets its address, routes and MTU.

**Evidence.** VpnService.Builder.establish() returns the TUN as a file descriptor (https://developer.android.com/reference/android/net/VpnService). WireGuard iOS finds the utun fd by scanning fds 0–1024 with getsockopt, because the older KVC lookup was killed by the iOS 15 sandbox, then hands the fd to Go via wgTurnOn (https://git.zx2c4.com/wireguard-apple/tree/Sources/WireGuardKit/WireGuardAdapter.swift, https://lore.kernel.org/all/A28ECC96-A28E-425E-90E3-E3C4D1B2E3E2@mullvad.net/T/). NEPacketTunnelNetworkSettings carries the address, included routes and MTU (https://developer.apple.com/documentation/networkextension/nepackettunnelprovider).

**Implication.** internal/overlay should accept an existing tun.Device or fd (wireguard-go's tun.CreateTUNFromFile already exists), and keep the address, route and MTU setup of §7.5 in the desktop daemon only. On phones, route only the overlay prefix (split tunnel), so other phone traffic is untouched.

#### 7. The iOS packet tunnel memory limit is about 50 MiB since iOS 15 and unchanged through iOS 26, but it is undocumented, lower on older devices, and counts the Go runtime.

**Evidence.** Quinn's table for iOS 16 gives packet tunnel 50 MiB (app proxy 15, DNS proxy 15). A DTS engineer said in Oct 2025 "AFAICT the limits are unchanged on iOS 26" and warned they "may well change" (https://developer.apple.com/forums/thread/73148?page=2). Tailscale in 2021, under the old 15 MB limit, ran at a 10–12 MB baseline after making GC more aggressive, setting GOMAXPROCS=1, reusing buffers and shrinking pclntab (https://tailscale.com/blog/go-linker). wireguard-go's iOS build shrinks its queues to 1024 and PreallocatedBuffersPerPool to 1024 with a 1700-byte MaxSegmentSize (https://raw.githubusercontent.com/WireGuard/wireguard-go/master/device/queueconstants_ios.go).

**Implication.** Bound every queue in bytes now: the pacing queues, the reorder buffer, and any per-packet buffer pools, under one named constant. Note the replay window (8,192 counters, 1 KB per peer per key) is already small. The mobile build sets debug.SetMemoryLimit to about 32 MiB and a small GOMAXPROCS. Phones need no 160,000 packets/s sizing.

#### 8. Probing every lane every 5 s while idle keeps a phone's cellular radio in its high-power state permanently. Cellular NATs keep idle UDP mappings for about a minute, so a much slower idle keepalive is enough.

**Evidence.** LTE RRC tail 11.576 s at a tail base power of 1060 mW, the longest of 3G, LTE and Wi-Fi; LTE measured up to 23 times less power-efficient than Wi-Fi (Huang, Qian et al., MobiSys 2012, https://web.eecs.umich.edu/~zmao/Papers/Thesis/JunxianHuangThesis2013.pdf; the 11.576 s and 1060 mW figures were seen only in a third-party copy, https://www.scribd.com/doc/228739748/4g-power-characteristics, so treat them as unverified against the official text). Cellular carrier NATs: median UDP mapping timeout 65 s (35 s for non-cellular), observed range 10–200 s (Richter et al., https://arxiv.org/pdf/1605.05606). A 5 s cadence is below the 11.6 s tail, so the radio never returns to idle.

**Implication.** Simplify the idle rule for all devices: when a peer is idle, keep one lane alive (the cheapest, unmetered if possible) with a probe every 25 s, and leave the other lanes dormant. The first data packet wakes every lane with the 200 ms burst. This also eases A8 (≤ 200 KB per lane per hour). Lanes need a metered flag that the platform supplies (Android: NOT_METERED capability; iOS: NWPath isExpensive).

#### 9. The owner's carrier, T-Mobile US, is IPv6-only with 464XLAT, so the phone's cellular IPv4 address is a local CLAT address (192.0.0.x) that no peer can reach. The real cellular lane is IPv6, or IPv4 through the carrier's NAT64.

**Evidence.** T-Mobile US went IPv6-only with 464XLAT (Internet Society case study, https://www.internetsociety.org/resources/deploy360/2014/case-study-t-mobile-us-goes-ipv6-only-using-464xlat/). In a packet tunnel on T-Mobile, the LTE source address was always 192.0.0.1. Apple's Matt Eaton: "XLAT464 has been enabled to provide IPv4 availability for cellular", and "your tunnel may only come up with IPv6 availability" (https://developer.apple.com/forums/thread/692319). Go's IsGlobalUnicast accepts 192.0.0.x, so §7.1's filter would publish it.

**Implication.** In §7.2, never publish candidates in 192.0.0.0/29. Probe IPv6 candidates alongside IPv4 ones (the spec already opens per-family sockets). Check whether Xanth's home connection has IPv6: if it does, a phone-to-Xanth cellular lane can skip T-Mobile's NAT64. That is unverified for the owner's ISP.

#### 10. Apple's built-in multipath features (Multipath TCP, Wi-Fi Assist) cannot carry wirebond's lanes.

**Evidence.** Apple: Multipath TCP "requires the destination server to have Multipath TCP enabled", it is TCP-only, and Wi-Fi Assist "prevents flows from using cellular data when your app is in the background" and "limits the amount of data" sent over cellular (https://developer.apple.com/documentation/foundation/improving-network-reliability-using-multipath-tcp). Aggregate mode: "This option is available only for experimentation" and needs a device in Developer mode (https://developer.apple.com/documentation/foundation/urlsessionconfiguration/multipathservicetype-swift.enum/aggregate).

**Implication.** Avoid. wirebond's own UDP lanes plus explicit interface pinning are the only route on iOS. Do not plan around Network.framework multipath service types.

#### 11. A phone relaying a laptop's traffic over a shared LAN, without tethering, is dominated by USB Personal Hotspot, which the spec already uses.

**Evidence.** The relay stops when the phone locks (finding 2; the 756941 thread is exactly this relay design). Hotel Wi-Fi often isolates clients from each other, which blocks laptop-to-phone traffic on the same network; that prevalence comes from a vendor and is unverified (https://www.purple.ai/en-us/guides/is-hotel-wifi-safe). Carriers count it as tethering: T-Mobile's terms have a heavy-tethering clause (https://conductatlas.com/platform/t-mobile/t-mobile-terms-and-conditions/provision/CA-P-039077/prohibited-use-heavy-tethering-usage-pattern/), and in 2015 T-Mobile said it can detect concealed tethering (https://www.fierce-network.com/wireless/t-mobile-to-crack-down-customers-who-abuse-mobile-hotspot-data-allowance-for-tethering). The owner measured Mac to iPhone over the hotspot at about 5 ms (CONTEXT.md).

**Implication.** Avoid building a relay mode. The laptop's cellular lane stays the USB hotspot link (§7.1 already re-binds when its index changes). The phone-as-device work exists for the phone's own traffic, such as Moonlight on the phone over Wi-Fi plus cellular.

#### 12. Both credible ways to build Go for phones need cgo. Tailscale's Android app uses gomobile bind; WireGuard's iOS app uses a c-archive with a patched Go runtime. gomobile has no tagged release but is used in production.

**Evidence.** Tailscale replaced its JNI/Gio layer with gomobile in May 2024 (https://tailscale.com/blog/android), built with `gomobile bind -target android -androidapi 26` (https://github.com/tailscale/tailscale-android/pull/689.diff). golang.org/x/mobile has only untagged pseudo-versions (Oct 2026) (https://pkg.go.dev/golang.org/x/mobile/cmd/gomobile). WireGuard's iOS Makefile runs `go build ... -buildmode c-archive` with CGO_ENABLED=1, after patching a copy of GOROOT (https://github.com/WireGuard/wireguard-apple/blob/master/Sources/WireGuardKitGo/Makefile). gomobile bind accepts only simple types (strings, numbers, []byte, interfaces).

**Implication.** Keep CGO_ENABLED=0 for desktop releases. Add one `mobile` Go package whose API uses only strings, ints, []byte and one callback interface (bind socket, protect, log), so the same package builds with gomobile bind for Android and as an iOS xcframework. Avoid runtime patches by handling wake in code (finding 2).

#### 13. iOS cannot satisfy R2, R3 or R4 as written. Distribution goes through Apple, and builds expire.

**Evidence.** Guideline 2.5.2: apps may not "download, install, or execute code which introduces or changes features or functionality of the app" (https://developer.apple.com/app-store/review/guidelines/). TestFlight builds become unavailable after 90 days, with up to 100 internal testers (https://www.developer.apple.com/help/app-store-connect/test-a-beta-version/testflight-overview). The packet tunnel needs the com.apple.developer.networking.networkextension entitlement (NEPacketTunnelProvider docs). Unverified: guideline 5.4, VPN apps only from organization accounts (that section was truncated). The owner's repo already ships an Expo app to TestFlight with EAS, so an Apple developer account exists (git log: "build the app for TestFlight with EAS").

**Implication.** Write phone-specific requirements: install from TestFlight or a sideloaded APK, updates through the store or TestFlight, and uninstall equals deleting the app (the OS removes the VPN profile). Plan a TestFlight re-upload at least every 90 days, for example from CI.

#### 14. An agent or human can operate a phone device almost entirely from a desktop, because both ends of a lane measure it. The phone needs no CLI and almost no UI.

**Evidence.** The spec's measurement is two-sided: each sender keeps per-lane RTT, OWD, loss and capacity from echoes (§7.3), so `wirebond status` on the Mac or Xanth already shows every lane to the phone. Invites are a single `wb1:` string (§6.2), which a phone can take as pasted text or a QR code. The phone shell's code size is an estimate only.

**Implication.** Keep the phone app minimal: paste or scan an invite, an on/off toggle, the overlay address and the lane count. Everything else, including mode and diagnosis, is driven from desktop `status` and records. Proposed order: Android first; iOS only after the spike in finding 3 passes.

### Proposals

- Replace the phones bullet in §15 with 'Phones later, seams now': phones run wirebond only while the VPN is on and the device is awake, inside one mobile Go package hosted by a thin Kotlin or Swift app, and the core keeps the five seams below so that package needs no core changes.
- Seam 1 (§7.1): link discovery is an interface (`Links() []Link`): the desktop daemon implements it with net.Interfaces(), and a phone host supplies links from Android's NetworkCallback or iOS's NWPathMonitor (Go's net.Interfaces fails on Android 11+, golang/go#40569).
- Seam 2 (§7.1): every lane socket is opened through one `bind(fd, link)` hook: the desktop uses the IP_BOUND_IF / SO_BINDTODEVICE / IP_UNICAST_IF table, Android calls Network.bindSocket plus VpnService.protect, and iOS uses whatever the spike proves (IP_BOUND_IF or NWConnection with requiredInterface).
- Seam 3 (§7.5): internal/overlay accepts an existing tun.Device or file descriptor, and the address, route and MTU commands in §7.5 move into the desktop daemon, because on phones the OS's tunnel settings do that.
- Seam 4 (§7.3): add a wake rule: after a platform wake event, or a gap of more than 1 s in the run loop's monotonic progress, treat every link as changed (200 ms probing for 10 s) and discard RTT and OWD samples that span the gap.
- Seam 5 (§8): bound every buffer in bytes by one named constant (pacing queues, reorder buffer, buffer pools), and have the mobile build call debug.SetMemoryLimit(32 MiB) with a small GOMAXPROCS, to stay under iOS's undocumented ~50 MiB packet tunnel limit.
- §7.3 idle cadence: when a peer has been idle for 5 s, probe only one lane every 25 s (the one with the lowest RTT, preferring unmetered) and leave the others dormant until the first data packet wakes them with the 200 ms burst, instead of probing every lane every 5 s.
- §6.3 LinkInfo: add `Metered bool`, supplied by the platform (Android: no NOT_METERED capability; iOS: NWPath.isExpensive; desktops: false), used only to choose the idle keepalive lane.
- §7.2: never publish candidate addresses in 192.0.0.0/29 (464XLAT CLAT, used by T-Mobile), and always probe IPv6 candidates alongside IPv4 ones.
- §6.5: `wirebond set endpoint host:port` also adds host:7849 as a TCP control address, and CONTEXT.md's owner steps forward both TCP and UDP 7849, so devices that cannot run Tailscale at the same time (any phone running wirebond) still reach the control channel.
- §4.2: say that CGO_ENABLED=0 applies to desktop releases, and that the future mobile package is built with cgo (gomobile bind for Android, an xcframework for iOS) and exposes only string, int, []byte and one callback interface.
- §1 and §15: add phone requirement rows: install through TestFlight (re-uploaded at least every 90 days) or a sideloaded APK, no self-update on iOS (App Review guideline 2.5.2), and uninstall equals deleting the app.
- §15: state that wirebond does not relay a laptop's traffic through a phone over a shared LAN, and that the laptop's cellular lane is USB Personal Hotspot, because an iOS tunnel stops when the phone locks, hotel Wi-Fi often isolates clients, and carriers count relaying as tethering.
- §14: add a step 5, Android first (it relies only on documented APIs), and then iOS only after a one-day spike proves a packet tunnel can send UDP over Wi-Fi and T-Mobile cellular at the same time on the owner's iPhone.


## Congestion control and capacity estimation

### Summary

1. The spec's aggregate-mode AIMD (×0.85 of its own rate per RTT, +1% of capacity per 50 ms) fails its own A4 test against the simulator's own spike model. In my fluid model (one hotel lane, 38 Mbps, spikes every 5±2 s, 8 seeds), the hotel queue's p95 is 97 ms. A4 requires under 20 ms. The cause: after a spike, a 0.85 cut leaves only about 15% headroom to drain a 150 ms queue. The fix that works is GCC's rule: cut to a fraction of the *measured delivery rate*, not of your own rate. Make the cut proportional to the delay excess and never more than half, as LEDBAT++ does. That gave a p95 of 20–22 ms at 24–30 Mbps.
2. On cellular, a stale capacity hurts a lot. With an 80→20 Mbps step, the spec's ×0.85-per-RTT gives a queue p95 of 179 ms and 1.9% loss. Cutting to the delivery rate gives 23–26 ms and 0.2–0.3%. Sprout's paper documents that cellular link speeds change dramatically with time.
3. Rate and capacity are welded together. The rate is boxed to 0.1–1.1× capacity, and capacity is learned only when the lane is congested. So there is no startup: going from 1 to 80 Mbps by 10% ratchets takes about 23 s by arithmetic, and A4 counts goodput from 5 s. Jitter-flagged "congested" samples can also lock in a low capacity. Use BBR's rule instead: capacity is the max of delivery-rate samples that were not app-limited. Add a ×1.25-per-RTT startup, and raise the rate only when wirebond's own queue is backlogged. Without that gate the rate drifts to 1.1× (41.8 Mbps on a 38 Mbps lane).
4. A 10-minute base OWD breaks in three ways. (a) A base-delay increase takes 10 minutes to absorb (RFC 6817 §2.5). (b) Clock skew turns into false queueing delay: 15–50 ppm is 9–30 ms over 10 minutes. (c) macOS and Linux monotonic clocks stop during sleep, so a Mac waking from sleep looks to its peer like hours of queueing. Use queueing delay = min(OWD − base OWD, RTT − min RTT). RTT is on one clock, so it bounds skew and sleep steps. Use 10 s windows as BBR and Copa do, and reset on lane down.
5. Two measurement bugs in §7.3. RecvPackets/RecvBytes are snapshotted when the report is sent, not when probe EchoSeq arrived, so the sent and received windows differ by the variation in EchoDelay. With ±8 ms jitter that is ±3.2% loss error over 500 ms, larger than the 2% threshold. The delivery-rate interval is also not defined on the receiver's clock. Snapshot the counters at EchoSeq's arrival. Time the window with (SentAt − EchoDelay), and cap the rate at the send rate, as the delivery-rate draft (draft-cheng) does.
6. The fixed thresholds sit inside Wi-Fi noise. With a 10 ms target my model starves to about 6 of 38 Mbps. The measured hotel ping deviation is 60 ms, with a 366 ms max, against the simulator's ±8 ms. Standard delay-based controllers use 60–100 ms targets, and GCC adapts its threshold. Make the target adaptive per lane, and replay the measured jitter in netsim.
7. Primary mode has no pacing, so any TCP flow inside the overlay fills the hotel or cellular buffer. Measured: 16.5 ms idle becomes 105/196 ms under load. The lane then reads "congested" for good, hedging copies everything (doubling cellular bytes), and game packets wait behind the download. Pace the primary lane as well, and keep the queue inside wirebond with fq_codel over the 256 buckets the spec already defines (RFC 8290). This is the single biggest win for competitive gaming, and it lets both modes share one controller.
8. Keep earliest-arrival scheduling: it is ECF, the literature-backed fix for MPTCP's minRTT scheduler. Probe every 20 ms while a lane carries data (about 52 kbps). In my model that raised hotel goodput from 24 to 30 Mbps at a similar p95.
9. Do not adopt Sprout, Verus, Copa, BBRv3, SCReAM or LEDBAT whole. Take three ideas: decrease to the delivery rate (GCC), 10 s minimum windows (BBR, Copa), and delivery samples that are not app-limited (BBR). The resulting controller is about 30 lines.

All simulation numbers are from my own deterministic fluid model, not from wirebond's netsim. Confirm them there before tuning.

### Findings

#### 1. The spec's aggregate-mode AIMD (on overuse ×0.85 of its own rate, at most once per RTT) cannot drain a spike queue fast enough to pass A4's 'hotel shaper queue p95 < 20 ms' under the spike model in SPEC §12.1. Cutting to a fraction of the measured delivery rate, proportional to the delay excess and capped at half (GCC plus LEDBAT++), passes.

**Evidence.** My own fluid model, 1 ms steps, 8 seeds: one hotel lane with a 38 Mbps token bucket, 180 ms buffer, 30 ms base OWD, ±8 ms jitter, and spikes every 5±2 s that add 150 ms over 20 ms. Saturating sender, probes every 50 ms, threshold 20 ms. Spec rule: 30.9 Mbps, queue p95 97 ms, 8.9% of time above 20 ms. 'rate = min(0.85·rate, 0.85·delivery rate over 500 ms)': p95 82 ms. Same rule with a 100 ms delivery window: 26.5 Mbps, p95 39 ms. 'rate = min(0.85·rate, deliveryRate100ms · max(0.5, 1 − (qd − 20)/(2·qd)))': 24.4 Mbps, p95 20.0 ms. With 20 ms probes: 30.5 Mbps, p95 22 ms. Floor with almost no own traffic: about 2.9% of time above 20 ms, so A4 has only about 2 points of slack. Without spikes all three rules give p95 of 12–13 ms. Why the spec rule fails: after one 0.85 cut from about C the lane drains at only 0.15·C, so a 150 ms queue takes about 1 s to clear. GCC: on overuse the estimate becomes beta × measured incoming rate, beta 0.85 recommended, range 0.8–0.95 (draft-ietf-rmcat-gcc-02 §5.5, https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02). LEDBAT++ decrease is proportional to delay/target and capped at W/2 (§4.2, https://www.ietf.org/archive/id/draft-irtf-iccrg-ledbat-plus-plus-06.html). Script: /private/tmp/claude-502/-Users-tristancopley-platform--claude-worktrees-rippling-rpass-comparison-3b1757/c885543c-b9a7-41fa-a790-26dd7b432ec1/scratchpad/ratecontrol_sim.py. Fluid model only; unverified in netsim.

**Implication.** Adopt: decrease to a fraction of the measured delivery rate over a short window (about 100 ms), with a cut proportional to the delay excess and floored at 0.5. Measure A4's p95 in netsim, and say whether it is over time or over packets.

#### 2. A remembered capacity that has gone stale on cellular, such as a rate drop from 80 to 20 Mbps, overloads the lane for seconds under the spec's ×0.85-per-RTT rule. Cutting to the delivery rate converges in one step.

**Evidence.** Same model, cell lane: 45 ms OWD, ±10 ms jitter, RTT 95 ms, capacity drops 80→20 Mbps at t = 10 s, estimate starts at 80. Spec: queue p95 179 ms (pinned at the 180 ms buffer), loss 1.9%. Delivery-rate cut: p95 25.6 ms, loss 0.3%. Proportional cut: 22.8 ms, 0.2%. Arithmetic: 0.85^n = 0.25 needs n ≈ 8.5 RTTs, about 0.8 s at 95 ms. Sprout's abstract says cellular 'link speeds change dramatically with time' and that existing protocols build multi-second queues; Sprout cut self-inflicted delay 7.9× against Skype (https://www.usenix.org/conference/nsdi13/technical-sessions/presentation/winstein). Cellular bufferbloat up to seconds of RTT: Jiang et al., IMC 2012 (https://techrep.csc.ncsu.edu/2012/TR-2012-6.pdf).

**Implication.** Adopt the same decrease-to-delivery-rate rule for every lane. Do not trust remembered capacity for more than the initial rate. A spec number held for a 'daemon lifetime' must never act as a ceiling.

#### 3. §9.2 boxes the rate to 0.1–1.1× capacity, and §7.3 learns capacity only when the lane is congested. Together they give no startup, slow discovery, and a capacity that can lock low. Delivery-rate samples taken while wirebond was backlogged (not app-limited) are the standard capacity estimator.

**Evidence.** Arithmetic from the spec: with capacity unknown, the bound is 1.1 × the lower bound, and the rate climbs 1% of the lower bound per 50 ms, so each 10% ratchet takes about 0.5 s. From 1 Mbps (probes and urgent traffic only) to 80 Mbps: ln 80 / ln 1.1 ≈ 46 steps ≈ 23 s, while A4 measures goodput from 5 s. Jitter-flagged 'congested' samples during app-limited traffic, for example 20 Mbps of video on the 38 Mbps hotel lane, set capacity to 20, capping the rate at 22. App-limited drift in my model: a 20 Mbps offered load on 38 Mbps for 30 s; the rate ends at 41.8 Mbps (1.1 × 38) with no gate and stays at 34.2 with a gate. BBR marks app-limited samples (draft-cheng-iccrg-delivery-rate-estimation §2.3, https://datatracker.ietf.org/doc/html/draft-cheng-iccrg-delivery-rate-estimation). BBR keeps a 10-round max-bandwidth filter and a 10 s min_rtt filter (tcp_bbr.c, https://raw.githubusercontent.com/torvalds/linux/master/net/ipv4/tcp_bbr.c) and has a 2.77 startup gain (draft-ietf-ccwg-bbr-07 §2.5, https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-bbr).

**Implication.** Simplify. Capacity is the max, over about 10 s, of delivery-rate samples taken while the lane's pacing queue was non-empty. Remember it per link only to seed the start. Add a startup (×1.25 per RTT until the first overuse). Increase only while backlogged. Drop the 0.1–1.1× box and the 'congested for 1 s' rule.

#### 4. Base OWD as the minimum over 10 minutes (§7.3) turns three ordinary events into false queueing delay lasting up to 10 minutes: a base-delay increase (route change or cellular handover), clock skew, and laptop sleep. Each one makes a lane look congested, so primary mode keeps hedging and aggregate mode drops the lane to its rate floor.

**Evidence.** RFC 6817 §2.5: 'If the actual base delay increases, however, a LEDBAT sender will take BASE_HISTORY minutes to adapt.' Appendix A.2: skew is 'a linearly changing error' and needs correction when the receiver's clock runs faster (https://www.rfc-editor.org/rfc/rfc6817.html). Skew arithmetic: 15 ppm × 600 s = 9 ms; 50 ppm = 30 ms, already the primary-mode congestion threshold. Sleep: Go's monotonic clock stops while macOS sleeps (https://github.com/golang/go/issues/66870, https://github.com/golang/go/issues/24595), and Linux CLOCK_MONOTONIC 'does not count time that the system is suspended' (https://man7.org/linux/man-pages/man2/clock_gettime.2.html). After the Mac sleeps for S seconds, the peer's OWD samples for the Mac's probes jump by S, and §7.3 resets base OWD only on an address change. LEDBAT++ moved to RTT because one-way timestamps lack a common clock frequency and compensating is 'very fragile' (§3.5, §4.5). Copa uses the minimum RTT over 10 s 'to handle route changes' and the minimum over srtt/2 for the standing queue, against ACK compression and wireless jitter (https://people.csail.mit.edu/venkatar/copa.pdf §2). BBR's MinRTTFilterLen is 10 s.

**Implication.** Define queueing delay = min(OWD − baseOWD, RTT − minRTT), with both bases as minima over about 10 s, and reset both when a lane goes down. RTT is measured on one clock, so it bounds skew and sleep steps, and the min() keeps OWD's direction specificity. This removes the need for the ten one-minute buckets.

#### 5. §7.3's loss and delivery-rate windows are misaligned. RecvPackets and RecvBytes are cumulative at the moment the report is sent, but the sender's count is taken at probe EchoSeq, so each window is off by the change in EchoDelay. The delivery-rate interval has no defined clock.

**Evidence.** Arithmetic: probes run on independent 50 ms timers, and Wi-Fi arrival jitter of ±8 ms moves EchoDelay by up to 16 ms between reports. The error over a 500 ms loss window (LaneView.Loss) is up to 16/500 = 3.2%, above the 2% congestion threshold in §9.1 and §9.2, so false 'loss' appears. Per 50 ms report the error is about ±30%. The delivery-rate draft computes rate = data_acked / max(ack_elapsed, send_elapsed), so delivery is capped by the send rate to reject aggregation and compression bursts (draft-cheng §2.2.4, link above). Every needed timestamp already exists: the receiver's arrival time of EchoSeq, on its own clock, is report.SentAt − report.EchoDelay.

**Implication.** Fix the spec. The receiver snapshots its cumulative counters when each probe arrives and reports the snapshot taken at EchoSeq. The delivery interval is Δ(SentAt − EchoDelay) on the receiver's clock. Delivery rate = ΔRecvBytes / max(Δrecv interval, Δsend interval of the matching probes).

#### 6. The fixed delay thresholds (10 ms in §7.3, 20 ms in §9.2, 30 ms in §9.1) sit inside Wi-Fi delay noise. The simulator's hotel jitter (±8 ms) is far smaller than the owner's measurement.

**Evidence.** CONTEXT.md: 100 idle pings on the hotel Wi-Fi gave a 32 ms average, 366 ms max and 60 ms deviation, against SPEC §12.1's ±8 ms. In my model a 10 ms target starves the lane to 5.9 of 38 Mbps (min-based base plus ±8 ms noise reads up to 16 ms of fake queue). Standardized delay controllers sit far higher: RFC 6817 TARGET ≤ 100 ms, LEDBAT++ 60 ms ('cannot be set too low'), SCReAM QDELAY_TARGET_LO 0.1 s (RFC 8298 §4.1.1.1, https://www.rfc-editor.org/rfc/rfc8298.html). GCC adapts its threshold: initial 12.5 ms, K_u 0.01 > K_d 0.00018, clamped 6–600 ms, and declares overuse only when held 10 ms (draft-ietf-rmcat-gcc-02 §5.4). It groups packets sent within 5 ms (burst_time) for channel-outage bursts. Unverified hypothesis: some of the Mac's Wi-Fi spikes come from AWDL channel hopping (time-shared availability windows; Stute et al., MobiCom 2018, https://arxiv.org/abs/1808.03156).

**Implication.** Measure before tuning. Make netsim replay the measured hotel ping distribution. Set each lane's target to max(20 ms, 2 × that lane's OWD deviation while app-limited), and require two consecutive over-target samples. Drop the separate 10 ms 'congested' test in §7.3, since capacity no longer needs it.

#### 7. Primary mode does not pace and has no queue of its own. Any TCP flow inside the overlay (a game download, a sync) fills the bottleneck buffer. The lane then looks congested indefinitely, hedging copies all bulk traffic (doubling cellular bytes), promotion never settles, and game packets queue behind the download.

**Evidence.** CONTEXT.md measured networkQuality on the hotel: 16.5 ms idle, 105 ms (down) and 196 ms (up) under load. That is about 89 ms × 37.7 Mbps ≈ 420 KB and 180 ms × 41.9 Mbps ≈ 940 KB of buffer, all of which a loss-based TCP fills. The §9.1 congestion rule (queueing delay above 30 ms for 500 ms) is then always true. Promotion needs the hedge lane to stay uncongested at the whole offered rate, but TCP grows to fill that lane too, so the hedge state persists. Fix precedent: fq_codel (RFC 8290, https://www.rfc-editor.org/rfc/rfc8290.html; CoDel target 5 ms, interval 100 ms, RFC 8289) at a shaper set just below the bottleneck moves the queue to where it can be managed (CAKE, https://arxiv.org/abs/1804.07617). wirebond already hashes flows into 256 buckets (§8.3).

**Implication.** Adopt one data path for both modes. Every lane has a paced queue at the controller's rate, with urgent packets first, then DRR across the 256 buckets with CoDel drops. 'Primary congested' becomes 'the primary's own queue is persistently backlogged' (offered > rate), not 'the network queue is high'. This most directly serves competitive gaming and is simpler than two signal paths.

#### 8. Keep the earliest-arrival rule in §9.2, arrival(l) = OWD(l) + (Backlog + size)/Rate(l). It is the published ECF scheduler, which beats MPTCP's default lowest-RTT scheduler on heterogeneous paths, especially for video.

**Evidence.** Lim, Nahum, Towsley, Gibbens, 'ECF: An MPTCP Path Scheduler to Manage Heterogeneous Paths' (CoNEXT/SIGMETRICS 2017, https://core.ac.uk/download/161895343.pdf). The default minRTT scheduler under-uses the fast path, and ECF 'consistently utilizes all available paths more efficiently… particularly for streaming video'. Caveat from the follow-up work: ECF as published uses at most two subflows (https://ipsj.ixsq.nii.ac.jp/record/221403/files/IPSJ-Z84-4ZC-03.pdf). wirebond's per-lane form generalizes to N lanes. Own reasoning: OWD feedback lags about one OWD plus one report interval (80–100 ms), so the formula is only as good as the pacing that keeps network queues small.

**Implication.** Keep it. Use the current queueing-inclusive OWD, not the base OWD. Skip a lane when its arrival would exceed the fastest lane's arrival plus the reorder timer budget, so A5's latency bound holds by construction.

#### 9. Probing every 20 ms while a lane carries data makes the controller react faster at negligible cost. The 50 ms cadence costs throughput.

**Evidence.** My model, proportional-cut controller: 50 ms probes give 24.4 Mbps at p95 20.0 ms; 20 ms probes give 30.5 Mbps at p95 22.1 ms. Spec controller: 30.9 → 33.2 Mbps (p95 still 90 ms). Cost: a probe is 75 + 54 = 129 bytes, so 50 per second is about 52 kbps per lane per direction, 0.14% of 38 Mbps. A8 (idle) is unaffected because idle cadence stays 5 s.

**Implication.** Change the active probe cadence from 50 ms to 20 ms (one named constant).

#### 10. The hotel looks like a buffered token-bucket shaper (delay rises before loss), but traffic policers that drop without queueing are common. The loss trigger and the decrease-to-delivery-rate rule handle both, and BBRv1's lt_bw is the precedent.

**Evidence.** From the loaded latencies above, the hotel buffers about 89–180 ms, so it is a shaper with a queue. Flach et al., SIGCOMM 2016 (https://research.google/pubs/an-internet-wide-analysis-of-traffic-policing/): loss is on average 6× higher on policed traces; the authors' slides put it at about 2% → over 20%, with up to 7% of lossy segments policed (https://archive.nanog.org/sites/default/files/20170201_Flach_An_Internet-Wide_Analysis_v1.pdf). BBRv1's policer detector (tcp_bbr.c) declares a policer after 2 intervals of at least 4 RTTs each with loss above 20% (50/256) and consistent bandwidth (ratio ≤ 1/8 or ≤ 4 kbit/s apart), then uses that rate for 48 RTTs. BBRv3: LossThresh 2%, Beta 0.7 (draft-ietf-ccwg-bbr-07 §2.8). Burst passage: a 64 KB burst inflates a 1 s delivery average by 0.5 Mbps (1.4%) and a 200 ms average by 2.6 Mbps (7%), so the 1 s hold in §7.3 does defeat burst-based overestimates.

**Implication.** Keep the 2% loss trigger (it matches BBRv3). No separate policer detector is needed if a decrease sets rate ≤ the measured delivery rate. Add a policer variant (no queue, drop above the rate) and a 180 ms buffer limit to netsim's hotel scenario.

#### 11. No published end-to-end controller should be adopted whole. Their results depend on the network (Pantheon), and each carries machinery that wirebond does not need: mode switching, forecasting, window management, scavenger yielding. Three ideas from them are enough.

**Evidence.** Pantheon (Yan et al., USENIX ATC 2018, https://usenix.org/conference/atc18/presentation/yan-francis): relative performance varies widely with path; the FAQ says Sprout does well on the US cellular network it was designed for but not in India or Colombia (https://pantheon.stanford.edu/faq). Copa in production at Facebook Live: median app RTT 499 → 479 ms and goodput +16.3% for the best-connection group, against CUBIC (https://engineering.fb.com/2019/11/17/video-engineering/copa/); search results relaying the SQP paper (arXiv 2207.11857) say Copa sometimes had high P90 RTT on LTE and Wi-Fi (not checked against the paper). LEDBAT and LEDBAT++ are scavengers by design, and SCReAM targets 100–400 ms of queueing delay, wrong for gaming. Ideas worth taking: decrease to the measured incoming rate (GCC β 0.85), 10 s minimum windows (BBR, Copa), delivery samples that are not app-limited (BBR), and a min filter on delay samples (Copa's srtt/2).

**Implication.** Simplify. Spec one controller of about 30 lines, cite these sources in CONTEXT.md, and remove the LEDBAT base-history reference.

### Proposals

- §7.3: define queueing delay = min(OWD − base OWD, RTT − min RTT), with both bases as minima over the last 10 s, reset when a lane goes down or its address changes; drop the ten one-minute buckets and the RFC 6817 reference.
- §7.3 Probe: the receiver snapshots RecvPackets and RecvBytes when each probe arrives and reports the snapshot taken at probe EchoSeq, so the sender's and receiver's loss windows match exactly.
- §7.3: delivery rate = ΔRecvBytes ÷ max(Δ(SentAt − EchoDelay) between consecutive reports, Δ sender send time of the matching EchoSeq probes), per draft-cheng-iccrg-delivery-rate-estimation §2.2.4.
- §7.3: capacity = the maximum over the last 10 s of delivery-rate samples taken while the lane's pacing queue was non-empty; remembered per link name only to seed a lane's starting rate; drop the 'congested for 1 s' rule and the 10 ms / 1% test.
- §9.2 Rate, decrease: on overuse (queueing delay above target on two consecutive samples, or loss above 2%), at most once per RTT, set rate = min(0.85 × rate, max(0.5, 1 − (qd − target)/(2·qd)) × delivery rate over the last 100 ms).
- §9.2 Rate, increase: add 1% of the current rate per 20 ms only while the lane's pacing queue is non-empty; until the lane's first overuse, multiply by 1.25 per RTT instead (startup); remove the 0.1–1.1 × capacity box.
- §9.2: set each lane's delay target to max(20 ms, 2 × that lane's mean OWD deviation measured while app-limited), so a noisy Wi-Fi lane is not starved.
- §9.1: give primary mode the same per-lane paced queue and controller as aggregate mode, with urgent packets first, then DRR across the 256 flow buckets with CoDel (target 5 ms, interval 100 ms); 'primary congested' means the primary's own queue has been backlogged for 500 ms.
- §7.3: probe every 20 ms (not 50 ms) while data has flowed in the last 5 s.
- §9.2: in the arrival formula use the current queueing-inclusive OWD, and skip any lane whose arrival exceeds the fastest lane's arrival plus the reorder wait bound.
- §12.1 hotel scenario: set the shaper buffer to 180 ms (about 850 KB), replay the measured idle-ping distribution (deviation 60 ms, max 366 ms) instead of ±8 ms, and add a policer variant (drop above 38 Mbps, no queue).
- §12.1: add scenario 'cell step' (cellular 80 → 20 Mbps instantly with a stale remembered capacity), passing if the cell queue p95 is under 40 ms and loss under 0.5% within 2 s.
- §12.1: add scenario 'sleep' (one device's monotonic clock pauses 60 s while lanes go down), passing if no lane reports queueing delay above its target within 5 s of the lanes returning.
- §12.2 A4: say whether 'queue p95' is over time or over packets, and note that the spike model alone puts about 3% of time above 20 ms.


## Robustness testing

### Summary

1. Build the core without I/O, then test it with a seeded, single-threaded event-loop simulator. In Go this is the only lean way to get bit-exact replay. Making the whole Go runtime deterministic takes a forked runtime and WASM (Polar Signals), and even then it is only "mostly" deterministic. The other option is Antithesis, priced per core with annual reservations. Go's `testing/synctest` gives fake time, but it does not make scheduling deterministic, and real sockets stop its clock from advancing.
2. Use `testing/synctest` (Go 1.25+, `Sleep` in 1.27) only for the timer-heavy wiring: control reconnect backoff, the 30 s key grace period, the 5 s idle probe rate and the update check. Run it over `net.Pipe` and a fake `PacketConn` built on channels. quic-go's `testutils/simnet` already does this.
3. Pick faults with swarm testing. For each seed, turn each fault class on or off with probability 0.5. The swarm testing paper found 42% more distinct crashes this way (104 vs 73). FoundationDB's BUGGIFY and TigerBeetle's VOPR (1000x time, 1,024 cores running 24/7) use the same idea. Run each seed in two modes: safety (faults stay on, invariants checked after every event) and liveness (faults heal at time T, then recovery must happen within a time limit).
4. Replay real traces inside netsim instead of using netem. Mahimahi's trace format is one millisecond timestamp per line, each line a chance to deliver 1,500 bytes. A reader for it is about 30 lines of Go. Include `TMobile-LTE-driving`, since the owner's hotspot is on T-Mobile. Add the Pantheon problem links: token-bucket policers at 12, 60 and 108 Mbps, ACK aggregation, and drop-tail buffers from BDP/10 up to 1 BDP (BDP is the bandwidth-delay product). Add Starlink traces (LENS) with the 15 s handover spikes. No good public Wi-Fi latency trace exists, so build the hotel profile from the owner's own measurements, plus a profile for the Mac's AWDL (AirDrop) channel-hopping spikes.
5. Device sleep and clock jumps break things the spec does not cover. Go's monotonic clock stops during sleep on macOS and Linux, so after a wake every lane still looks fresh. Two features depend on the wall clock: the ordering of `PeerRecord.Issued` and the X.509 validity check. A Raspberry Pi 4 has no hardware clock, so its wall clock can be wrong after boot. Fix all three in the spec and test them in netsim.
6. NAT behavior to model: carrier NATs (CGN) drop idle UDP mappings after 10–200 s, with a median of 65 s on cellular networks. The QUIC interop runner's rebind scenario changes the client's port, or its address, every 5 s. That scenario has caught real bugs, such as a server that kept sending to the old port. Tailscale's natlab has four NAT types: one2one, easy, easyaf and hard.
7. Use Go's built-in fuzzing (no dependency) in three ways:
   - on the parsers;
   - checked against a naive reference model (replay window, reorder buffer);
   - as a scenario generator: the fuzz bytes are decoded into a script of network events, so coverage guidance searches for bad scenarios.

   Crashing inputs saved in `testdata/fuzz` run as regression tests on every plain `go test`.
8. Keep the netns + `tc` tier small, about 6 scenarios. Its job is to check the real kernel, TUN device and sockets, not the algorithms. netem jitter reorders packets unless a pfifo queue sits under it. netem also cannot jump the wall clock, and libfaketime does not work on a static Go binary, so clock faults belong in netsim.
9. Assert tails, not averages: frame completeness, p99 and p99.9 frame latency above the best lane's one-way delay, stutter count, urgent packets lost, longest bulk gap, time to fail over and to recover, primary changes per minute, duplicate overhead, and idle bytes per hour. Every failure must print a one-line command that reproduces it from its seed. That is what makes the harness usable by AI agents.
10. For phones: on LTE the radio stays in its high-power state for about 11.6 s after the last packet, at roughly 1 W. Waking an idle radio adds about 260 ms (secondary source). The spec's 5 s idle probe therefore keeps a phone's radio awake all the time. Model a "cold radio" delay in netsim and decide the idle cadence on purpose.

### Findings

#### 1. Bit-exact deterministic simulation is cheap in Go only if wirebond's data-plane core does no I/O of its own. Making the whole runtime deterministic is costly and still imperfect.

**Evidence.** Polar Signals got deterministic simulation in Go by compiling to wasip1/wasm on wasmtime (one OS thread), patching the runtime so its random seed comes from an env var (`GORANDSEED`, under 10 lines), and using `-tags=faketime`. It found 3 data-loss and 2 duplication bugs in FrostDB within weeks. They call it '(Mostly)' deterministic: some runs with the same seed diverge, fake time can loop forever, and they maintain a forked runtime (https://polarsignals.com/blog/posts/2024/05/28/mostly-dst-in-go). Antithesis does not publish prices; cost is per core, with annual reservations of 24+ or 48+ cores (https://antithesis.com/pricing). FoundationDB and TigerBeetle get determinism by running the whole cluster in one thread, with the clock, network and disk replaced by a seeded simulation (https://docs.tigerbeetle.com/concepts/safety/).

**Implication.** Adopt the no-I/O design: `lane`, `sched`, `reorder` and `wire` (including real ChaCha20-Poly1305 sealing) expose `Handle(now, event) -> outputs` and `NextDeadline()`, with no goroutines, no `time.Now` and no global random source. `netsim` owns the single clock, a seeded PCG random generator (`math/rand/v2`) and an event heap. Add a determinism self-test: run seed S twice and compare the hashes of the event logs. It catches Go's randomized map iteration order, leaked `time.Now` calls and stray goroutines. Avoid WASM, runtime forks and Antithesis.

#### 2. `testing/synctest` gives fake time to real goroutine code, but not deterministic scheduling, and real sockets stop its clock. Use it for the daemon wiring, not the data plane.

**Evidence.** pkg.go.dev/testing/synctest: `Test` was added in go1.25.0 and `Sleep` in go1.27.0. 'Time in a bubble only advances when every goroutine in the bubble is durably blocked.' Channel operations, `select` on bubble channels, `sync.Cond`, `WaitGroup` and `time.Sleep` count as durably blocked. Mutexes, network I/O and system calls do not, and the docs say a goroutine blocked on network I/O keeps the bubble from going idle. Their example uses `net.Pipe` instead (https://pkg.go.dev/testing/synctest@master). quic-go ships `testutils/simnet` (an in-process `net.PacketConn` with per-link latency and MTU, plus a `simnet_synctest_test.go`) (https://github.com/quic-go/quic-go/tree/master/testutils/simnet).

**Implication.** Use synctest for the control channel's 1–60 s reconnect backoff, the `hello`/generation handshake, the 30 s grace period for the previous key, the switch between 50 ms and 5 s probing, and the daily update check. Each test then runs in milliseconds of wall time. This needs `internal/link` and `internal/control` to accept an injected `net.PacketConn` and `net.Conn`, with a fake built on channels for tests. Do not count on synctest for replay: goroutine interleaving at the same instant can still differ between runs.

#### 3. Choose faults with swarm testing (a random subset of fault types per seed) and run every seed in both a safety mode and a liveness mode. This is the main idea behind FoundationDB's BUGGIFY and TigerBeetle's VOPR.

**Evidence.** Swarm testing (Groce, Zhang, Eide, Chen, Regehr, ISSTA 2012) leaves out each feature with probability 0.5 per test configuration. In a week it found 104 distinct compiler crashes versus 73 (+42%) (https://www.cefns.nau.edu/~adg326/issta12.pdf, https://blog.regehr.org/archives/591). Secondary sources describe FoundationDB's BUGGIFY as enabling each site once per run, after which it fires 25% of the time (https://pierrez.github.io/fdb-book/meet_fdb/correctness.html). TigerBeetle's VOPR runs at 1000x speed on 1,024 cores, 24/7 (https://docs.tigerbeetle.com/concepts/safety/). Its packet simulator settings are: exponential one-way delay (mean and minimum), packet loss probability, packet replay probability, partition mode (none, uniform size, uniform partition, isolate single), symmetric or asymmetric partitions, partition stability, a maximum number of in-flight packets per path, and path clogging probability and duration (https://raw.githubusercontent.com/tigerbeetle/tigerbeetle/main/src/testing/packet_simulator.zig).

**Implication.** Give netsim one `Faults` struct whose fields are drawn from the seed, each enabled with p = 0.5. Safety mode keeps faults on and checks invariants after every event. Liveness mode heals all faults at time T and asserts recovery within a time limit. On a PR, run a few hundred seeds in about 1–2 minutes; nightly, run as many seeds as the budget allows. A failure prints `go test ./internal/netsim -run Swarm -seed=N`, and the seed goes into a regression list.

#### 4. The standard public cellular traces are Mahimahi's 2013 Sprout set. The format is trivial to replay inside netsim, and there is a T-Mobile LTE trace, which matches the owner's carrier.

**Evidence.** https://github.com/ravinet/mahimahi/tree/master/traces holds `.up` and `.down` files for ATT-LTE-driving, ATT-LTE-driving-2016, TMobile-LTE-driving, TMobile-LTE-short, TMobile-UMTS-driving, Verizon-EVDO-driving, Verizon-LTE-driving and Verizon-LTE-short. Format (mm-link(1) man page): each line is a millisecond timestamp giving one chance to deliver 1,500 bytes; one line per ms is 12 Mbit/s; the trace loops at its end (https://manpages.ubuntu.com/manpages/jammy/man1/mm-link.1.html). The traces were recorded by driving around with a Saturator that kept the queue always full. The Sprout authors' slides report LTE holding packets for more than 10 s during a download (Winstein et al., NSDI 2013, https://www.usenix.org/conference/nsdi13/technical-sessions/presentation/winstein). The license of the trace files was not checked.

**Implication.** Add a Mahimahi trace reader to netsim (about 30 lines) as a link model alongside the constant-rate one. Use TMobile-LTE-driving as the 'cell' lane in a trace-driven variant of A1 and A2, and Verizon-LTE-short as a harsh case. Fetch the traces in CI with a checksum rather than committing them until their license is confirmed. In the netns tier, Ubuntu packages mahimahi if trace replay through the real kernel is ever needed.

#### 5. Pantheon's 'pathological' emulators cover the problem links a hotel hits, and Pantheon also shows how far even calibrated emulation is from reality.

**Evidence.** From https://pantheon.stanford.edu/faq: token-bucket policers with bottleneck rates of 12, 60 and 108 Mbps; ACK aggregation on the return path, either '1 ACK every 100 milliseconds' or '10 ACKs every 200 milliseconds'; and drop-tail buffers of 1 BDP, BDP/2, BDP/3 and BDP/10. Their calibrated emulators (constant rate, propagation delay, random loss, drop-tail) match real paths only to 'within about 17% on average' (ATC 2018 Best Paper, https://usenix.org/conference/atc18/presentation/yan-francis). Pantheon is no longer maintained.

**Implication.** Add these as netsim fault types: a policer with rate and burst (the hotel already uses 38 Mbps with a 64 KB burst), an aggregation mode that delivers in bursts every N ms, and buffer depth as a fraction of BDP (BDP/10 means early loss; 1 BDP or more means bufferbloat). Treat simulation passing as necessary but not sufficient. Keep the owner's real Moonlight run (§12.3) as the final gate, and expect errors on the order of 20% between emulated and real performance.

#### 6. Starlink and 5G traces exist, but at coarse granularity. Starlink's handover spikes come every 15 s at fixed times, which is a clean test of whether hedging overreacts.

**Evidence.** LENS (https://github.com/clarkzjw/LENS, MMSys 2024): latency traces from 13 Starlink dishes at 7 points of presence on 3 continents, as IRTT JSON and ping text, in monthly snapshots on Zenodo. The UVic group reports handovers every 15 s at :12, :27, :42 and :57 past each minute, synchronized globally (https://onlineacademiccommunity.uvic.ca/starlink/). arXiv 2601.08439 reports handover spikes of about 140 ms (seen only in a search snippet, not checked in the paper). 5G: uccmisl/5Gdataset is per-second throughput plus channel metrics from an Irish operator, static and in a car (https://github.com/uccmisl/5Gdataset). Lumos5G is per-second TCP downlink throughput over commercial mmWave in Minneapolis, CC-BY 4.0 (https://networking.umn.edu/lumos5g).

**Implication.** Add a 'starlink' profile: base delay from LENS, plus a spike of about 100–150 ms at :12, :27, :42 and :57. Assert that primary mode does not promote or flap on these periodic spikes (at most one primary change per minute, as in A3), and that urgent copies hide them. Convert the per-second 5G throughput traces into Mahimahi delivery times, noting that they lose all burstiness under a second; use them only to test capacity tracking (the 10%/min decay and promotion).

#### 7. There is no good public Wi-Fi latency trace for gaming. The owner's own client, a MacBook, has a known Wi-Fi spike source: AWDL channel hopping.

**Evidence.** The Register (2025-10-23) reports researcher Christoff Visser's finding: AWDL (Apple's AirDrop link) keeps listening and favors the 'social' channels 6, 44 and 149. Moonlight's test tool showed about 20 ms latency with about 25 ms variance, and Steam's tool bounced between 3 and 90 ms (https://www.theregister.com/2025/10/23/apple_airdrop_awdl_latency_research/). A Moonlight issue thread reports that `ifconfig awdl0 down` does not always help (https://github.com/moonlight-stream/moonlight-ios/issues/627). The owner's hotel measurements (CONTEXT.md): 16.5 ms idle; 105 ms (down) and 196 ms (up) under load; ping max 366 ms with deviation 60 ms; a 20 MHz channel. netem's `slot` option models bursty MACs such as Wi-Fi and LTE by holding packets and releasing them in slots (https://man7.org/linux/man-pages/man8/tc-netem.8.html).

**Implication.** Build the hotel and Wi-Fi profiles from the owner's numbers instead of looking for a public trace. Add an 'awdl' fault: periodic gaps of tens of milliseconds on the Mac's Wi-Fi link only (the period and length are not verified; measure them on the Mac). Add slot-style burst delivery to netsim's link model. Record real traces too: the 50 ms probe stream already measures each lane's one-way delay, RTT and loss, so a lane's delay and loss over time can be dumped and replayed. Capacity cannot be recovered this way without saturating the link.

#### 8. NAT state is short-lived and changes on its own. Rebinding tests from QUIC interop have caught real 'keeps sending to the old port' bugs.

**Evidence.** Richter et al., IMC 2016 (https://arxiv.org/pdf/1605.05606): idle UDP mapping timeouts of 10–200 s, 74% at 1 minute or less, with a median of 65 s for cellular carrier NATs and 35 s for non-cellular ones. The quic-network-simulator rebind scenario takes `--first-rebind=1s --rebind-freq=5s`, with `--rebind-addr` changing the client's IP as well as its port (https://github.com/quic-interop/quic-network-simulator/blob/master/sim/scenarios/rebind/rebind.cc; interop logs at https://interop.seemann.io/logs/latest/s2n-quic_quic-go/rebind-addr/output.txt). HAProxy's QUIC server kept sending to the old port after a rebind and timed out (https://github.com/haproxy/haproxy/issues/3329). Tailscale's natlab vnet defines one2one, easy (address-and-port filtering), easyaf (address-only filtering) and hard NAT types (https://pkg.go.dev/tailscale.com@v1.96.4/tstest/natlab/vnet).

**Implication.** Give netsim a NAT model about 100 lines long: one2one, easy and hard, a mapping timeout drawn from 10–200 s, and rebinding of port or address every 5 s. Extend A7 to run under rebinding every 5 s for 60 s and require the same 100 ms recovery each time. Check that the 5 s idle probe stays below the 10 s minimum timeout. Add an invariant that no packet is sent to a return address older than the newest authenticated probe.

#### 9. Device sleep makes Go's monotonic clock pause on macOS and Linux. After waking, lanes look fresh while their NAT mappings and addresses are stale. The spec has no wake handling.

**Evidence.** Go issue #66870: monotonic time appeared to stop during sleep on darwin (Go 1.22). Issue #24595: Go uses CLOCK_MONOTONIC, which excludes suspend; Linux 4.17's change making MONOTONIC behave like BOOTTIME was reverted. Issue #35012: Windows' boot-time clock behavior (Go 1.13.3) was reverted at Microsoft's request (https://github.com/golang/go/issues/66870, https://github.com/golang/go/issues/24595, https://github.com/golang/go/issues/35012). Current per-OS behavior is not confirmed for the latest Go release. With the spec's liveness rule (a lane is down after 2 s without an echo, measured on the monotonic clock), a laptop that slept for 10 minutes judges its lanes by an echo that seems only milliseconds old.

**Implication.** Add wake detection to §7.3: when the wall clock advances more than the monotonic clock by over 1 s (Go's `time.Now()` carries both readings), mark every lane `suspect`, go back to the 200 ms probe rate, list the links again at once, and keep the old primary until a probe proves it. In netsim, model sleep as freezing one node's clock and timers while the rest of the world goes on. Test both clock behaviors (paused and advancing) and asymmetric sleep (only one side sleeps).

#### 10. Two parts of the spec depend on the wall clock and break on clock jumps: `PeerRecord.Issued` ordering, and X.509 validity checks in TLS. The Raspberry Pi, which has no RTC, is the likely trigger.

**Evidence.** SPEC §6.3: 'A newer Issued replaces an older one.' If a device's wall clock jumps backward, its new records lose to its own cached older ones, so its new link addresses never reach the other devices. Go's crypto/x509 checks NotBefore and NotAfter against `VerifyOptions.CurrentTime`, which defaults to now. A device whose clock is earlier than a certificate's NotBefore fails the TLS handshake. The Raspberry Pi 4 has no hardware RTC (the Pi 5 needs a battery for one), so it boots with a stale or 1970 clock until NTP syncs. No measured incident is cited here; this is a design analysis.

**Implication.** Set `Issued = max(time.Now(), the device's own last Issued + 1 ns)`, taking its own last record from `peers.json`. Issue every certificate with NotBefore at the Unix epoch and NotAfter at 99991231235959Z (RFC 5280's value for 'no expiry'), so trust never depends on the wall clock. In netsim, test wall-clock offsets of ±1 year and jumps during a run.

#### 11. Go's built-in fuzzing covers parsers and differential (model-checked) testing with no new dependencies. Decoding fuzz bytes into an event script also gives property-based testing guided by code coverage.

**Evidence.** From https://go.dev/doc/security/fuzz/: fuzz arguments can be `[]byte`, `string`, ints, floats and bools. Failing inputs are saved to `testdata/fuzz/FuzzXxx` and run as regression tests on every plain `go test`. Each run fuzzes one target (`-fuzz` must match one test). Each input has a 1 s timeout. Coverage instrumentation works only on amd64 and arm64. Targets must be deterministic and keep no state between calls. ClusterFuzzLite supports Go, with a 10-minute run per change and scheduled batch runs (https://google.github.io/clusterfuzzlite/running-clusterfuzzlite/).

**Implication.** Fuzz targets, each with a check:
- the packet header and open path: any bytes that are not authentic are dropped with no reply and no state change;
- `Probe` decoding: decode then re-encode round-trips;
- control framing and JSON (1 MiB limit);
- the `wb1:` invite token;
- the IPv4/TCP classifier, including fragments, IHL < 5 and truncated packets;
- the replay window checked against a model built on a map, under counters arriving out of order across 2–4 lanes;
- the reorder buffer checked against a sorted-delivery model;
- `FuzzScenario`: decode bytes into netsim events and check every safety invariant.

Nightly CI fuzzes each target in turn, for example 5 minutes each. Commit crashing inputs.

#### 12. Linux network namespaces with tc netem test the real kernel, TUN device and sockets, but netem has pitfalls and cannot fake clocks. Keep this tier small.

**Evidence.** tc-netem(8): with jitter, netem's internal queue sorts packets by send time and reorders them ('delay 100ms 75ms' sends the second packet first), unless a FIFO is attached under it ('parent 1:1 pfifo limit 1000'). `rate` makes artificial bursts because of kernel clock granularity. `slot` emulates DOCSIS, Wi-Fi and LTE burst delivery. Loss models include `gemodel` (Gilbert-Elliott) and a 4-state Markov chain. Netem combined with other queuing disciplines 'may not always work' (https://man7.org/linux/man-pages/man8/tc-netem.8.html). WireGuard's netns.sh is a precedent: three namespaces, iperf3 over IPv4 and IPv6, and a roaming test that changes the endpoint address and checks the peer follows (https://git.zx2c4.com/wireguard-rs/tree/netns.sh). Linux time namespaces offset only the monotonic and boottime clocks, not the wall clock, and libfaketime cannot hook a static Go binary built with `CGO_ENABLED=0`.

**Implication.** Limit the netns tier to about 6 scenarios that check what only real systems can show:
- hotel shaping with tbf at 38 Mbit and a 64 KB burst, plus netem 30 ms with a pfifo child;
- `ip link set down/up` flapping;
- `ip addr del/add` (DHCP renew, address change);
- rebinding by flushing conntrack under nftables masquerade (an nft equivalent of `--random`);
- `kill -STOP`/`-CONT` of one daemon for 10 s, a stand-in for sleep as the peer sees it;
- an MTU check with iperf3 through `wb0`.

All clock faults go to netsim.

#### 13. Phones on LTE pay about 1 W for 11.6 s after each packet, and an idle radio adds about 260 ms to the next packet. The spec's 5 s idle probe sits inside that window.

**Evidence.** Huang et al., MobiSys 2012, as summarized in secondary course notes and slides: LTE tail timer 11.576 s at about 1,060 mW; promotion from RRC_IDLE to RRC_CONNECTED takes 260 ms (from a lecture slide, not checked against the paper). The operator sets the tail time (https://www.cs.sjtu.edu.cn/~xue-gt/wireless/Paper/Energy%20efficiency%20of%20cellular%20network%20and%20smartphone/A%20Close%20Examination%20of%20Performance%20and%20Power.pdf). Carrier NAT minimum timeout is 10 s and the cellular median is 65 s (Richter 2016, above).

**Implication.** Add a 'cold radio' link fault to netsim: a delay of about 260 ms on the first packet after more than about 11 s idle. Assert that the first urgent packet after idle still arrives on time through its copy on a warm lane. For phones, idle probing on cellular links is a choice between battery and mapping lifetime. A 25–50 s cadence lets the radio sleep but risks hitting the 35–65 s median NAT timeout. Make it a named constant per link type and measure it. Do not assume 5 s.

#### 14. Metrics have to be about tails and frames to stand for competitive gaming and Moonlight. Means hide the failures that matter.

**Evidence.** CONTEXT.md: Moonlight discards a frame that is incomplete when the next frame's packets arrive. At 120 fps a frame lasts 8.3 ms, and audio tolerates about 30 ms of reordering. The owner's hotel ping had a 32 ms mean but a 366 ms max. The spec's A1–A8 already assert frame completeness and p99 latency. TigerBeetle and FoundationDB separate safety (always true) from liveness (holds after faults stop).

**Implication.** Metrics netsim must compute and assert:
- **Safety** (every event, every seed): no (key, counter) pair sealed twice; no inner packet delivered twice; nothing forged, corrupted or replayed is delivered; no reply larger than its request; a lane address changes only on a newer authenticated probe; queues and buffers stay within their limits; ordered data comes out in order, except late packets after a skipped gap.
- **Liveness** (after healing): a working lane within 1 s; a primary within 5 s; all queued urgent packets delivered.
- **Gaming**:
  - frame completeness at least 99.5%;
  - p99 frame latency at most best one-way delay + 10 ms, and p99.9 at most + 25 ms;
  - stutters (frames later than one frame interval) at most 1 per 1,000;
  - zero urgent loss while at least one lane is up;
  - longest bulk gap at most 300 ms on failover;
  - primary changes at most 1 per minute under periodic spikes;
  - duplicate overhead in primary mode at most 5% of bytes outside hedging (a proposed limit, not measured);
  - idle traffic at most 200 KB per lane per hour.
- **Allocations**: `testing.AllocsPerRun` = 0 on `Pick` and on the seal and send path.

### Proposals

- Rewrite §12 into three tiers: (1) unit and fuzz tests, (2) `netsim` deterministic simulation of the real no-I/O core plus `synctest` tests of the daemon wiring, (3) a Linux netns+tc smoke tier of about 6 scenarios. State that tiers 1 and 2 must run under plain `go test ./...` in under 2 minutes.
- In §4.1, require `lane`, `sched`, `reorder` and `wire` to have no goroutines, no `time.Now` and no global random source: each is driven by `Handle(now, event)` and reports `NextDeadline()`. Add a test that fails if those packages import `time.Now`, `math/rand` or `net`.
- Add a determinism test to §12.1: each netsim seed runs twice and the SHA-256 of the two event logs must match.
- Add swarm fault selection to §12.1: per seed, each fault class (loss with Gilbert-Elliott bursts, jitter, policer, drop-tail buffer depth, hotel spikes, AWDL gaps, Starlink handover spikes at :12/:27/:42/:57, cold-radio delay, flapping, one-way and two-way blackholes, duplication, corruption, replay, NAT rebinding of port or address, NAT mapping expiry, interface index or address change, link loss, device sleep, CPU stall, wall-clock jump) is on with probability 0.5.
- Run every netsim seed in safety mode (invariants after every event) and liveness mode (faults heal at T; a working lane within 1 s, a primary within 5 s), and have every failure print `go test ./internal/netsim -run Swarm -seed=N`.
- Add a Mahimahi trace link model to netsim and trace-driven variants of A1 and A2 that use `TMobile-LTE-driving` from github.com/ravinet/mahimahi/traces as the cell lane, fetched with a pinned checksum.
- Add acceptance A9 (sleep/wake): one device freezes for 10 min while the other runs on; traffic resumes within 1 s of wake, under both paused and advancing monotonic clocks.
- Add acceptance A10 (rebind storm): NAT rebinding of port, then address, every 5 s for 60 s; each recovery within 100 ms of the next probe, and no urgent loss while another lane is up.
- Add acceptance A11 (flapping): one lane flips up and down on a random cycle averaging 2 s for 5 min; primary changes at most 1 per minute, and the longest bulk gap is at most 300 ms.
- Add acceptance A12 (Starlink handover spikes): a 120 ms spike every 15 s on the primary; no promotion, and p99 urgent latency stays at most the other lane's one-way delay + 5 ms.
- Add acceptance A13 (clock jump): wall clock offset by ±1 year and jumped mid-run; the control channel stays up and peer records still propagate.
- In §6.3, set `Issued = max(time.Now(), the device's own last Issued + 1 ns)` so a backward clock jump cannot freeze a device's record.
- In §6.1, issue every certificate with NotBefore at the Unix epoch and NotAfter at 99991231235959Z, so TLS verification never depends on the wall clock.
- In §7.3, add wake detection: when the wall clock moves more than 1 s beyond the monotonic clock between ticks, mark all lanes suspect, probe at 200 ms and list the links again at once.
- In §7.3, make the idle probe interval a named constant per link type, and note that 5 s keeps an LTE phone radio in its ~1 W high-power state (the tail timer is ~11.6 s) while carrier NAT timeouts range from 10 s to 200 s.
- List the fuzz targets in §12: packet open, `Probe` decode, control framing, invite token, IPv4/TCP classifier, the replay window and reorder buffer against reference models, and `FuzzScenario` (bytes → netsim event script); crashing inputs are committed to `testdata/fuzz`.
- In §12.3, attach a `pfifo` child under every netem qdisc that has jitter, so the emulator does not cause the reordering itself, and use `kill -STOP/-CONT` to stand in for device sleep.
- In §12.2, add an assertion that `testing.AllocsPerRun` is 0 for `Scheduler.Pick` and the seal/send path, and add p99.9 frame latency and stutter count (frames later than one frame interval) to A1 and A5.


## CLI, install, update and size

### Summary

1. The CLI can shrink from 13 verbs to 8 with no lost capability: install [token], invite, revoke, status, set, log, update, uninstall. `init` goes away because the first `invite` creates the network. `join` goes away because `install <token>` joins. `mode` becomes `set mode=...` (Tailscale-style partial `set` that also prints the config). `version` becomes `--version` plus the header of `status`, and `daemon` is hidden. `invite` should print the whole command to paste on the new device, download included, so adding a device is one copy and paste.
2. One contract serves agents and humans. The `--json` output is exactly the daemon's socket reply `{ok,data}` / `{ok:false,error:{code,message,hint}}`, so it costs no extra code. Error codes are stable strings. Exit codes are 0, 1 and 2. Nothing prompts. Every command is idempotent and reports `changed`. `status` carries `schema:1` and a `problems[]` list with fixes. Tailscale's own `status --json` warns "format subject to change"; wirebond should promise additive-only changes instead.
3. `status` and `log` should work without sudo, as `tailscale status` does: the daemon checks the socket peer's credentials and only lets root change anything. Agents can then diagnose without a password. Never add a Tailscale-style "operator" who can change things without being root (see TS-2026-005).
4. The install line in SPEC 5.1 is wrong: it hardcodes arm64. Name the assets by `uname -sm` (Darwin-arm64, Linux-x86_64, Linux-aarch64, Linux-armv7l, Linux-armv6l) and by `$env:PROCESSOR_ARCHITECTURE` on Windows, so one line works on every machine. Go builds GOARM=7 by default when cross-compiling, so a Pi Zero or Pi 1 (armv6) cannot run the planned armv7 build. Build 32-bit ARM with GOARM=6.
5. Checking for updates through api.github.com is fragile: unauthenticated calls are limited to 60 per hour per IP, and hotels and carrier NAT share IPs. `github.com/<repo>/releases/latest` redirects to `/releases/tag/vX.Y.Z` (I checked this live). The tag comes from that redirect, with no JSON and no API. The asset and SHA256SUMS are then fetched from `/releases/download/<tag>/`, never from `latest` twice, because a release published between two `latest` requests would give an asset and a checksum file from different releases.
6. There is a real Windows bug in SPEC 5.3. Windows only runs the service's recovery actions (restart after 1 s) when the process ends without reporting SERVICE_STOPPED, unless the "non-crash failures" flag is set. A Go service that stops cleanly with exit code 3 is not restarted, so it stays down after an update. Fix: SetRecoveryActionsOnNonCrashFailures(true) plus three restart actions and a reset period, or os.Exit without reporting stopped. On macOS and Linux, re-exec in place (syscall.Exec), which depends on no service manager.
7. Make updates safe. Write `.new`, fsync it, run `.new version --json` and check that it prints the expected tag (this catches a wrong architecture, a corrupt file, antivirus quarantine or a code-signing kill), then rename it into place. Never write into the existing executable: macOS caches the code-signature check per file, and a binary changed in place is killed. Keep `.old` until the new daemon has run 60 s, and restore it after repeated crashes. Apply automatic updates only after 10 idle minutes, so an update never drops a game mid-match.
8. Size: the stripped Go 1.27 baseline on linux/amd64 is 1.22 MB for an empty program, 2.70 MB with crypto/tls and 3.54 MB with net/http (golang/go#81132). My estimate for wirebond is 4.5–6 MB, against 18 MB for a stripped Tailscale combined binary. Use `CGO_ENABLED=0 -trimpath -ldflags='-s -buildid='` (since Go 1.22 `-s` also drops the debug info that `-w` drops) and the `nethttpomithttp2` tag, which still exists. Add a CI size budget. No UPX: it shrank Tailscale from 18 MB to 4.5 MB, but packed files trip antivirus and weaken W^X memory protection.
9. Keep dependencies small. No self-update library (rhysd/go-github-selfupdate pulls in 12 modules, including go-github, oauth2 and appengine); the swap is about 60 lines of our own code. No cobra and no text/template: their runtime reflection on method names makes the Go linker keep every exported method. SPEC 4.2's list of 4 dependencies is incomplete: wireguard/tun pulls in wireguard/conn, which pulls in x/net on Linux.
10. Phones: the zero-install way to use a phone already works, since its hotspot over USB or Wi-Fi is a link. `status` should name it as such. Running wirebond on the phone itself needs a NetworkExtension or VpnService app. iOS memory limits for that are reported at 15–50 MB depending on the version; Tailscale fits Go in about 10–12 MB.
11. Mode names: "primary/aggregate" is jargon that describes the mechanism. I propose `stable` (default) and `combine`. "Speed" is a bad name: Speedify uses it for bonding, but gamers read it as latency. If a mode that copies every packet on every lane is ever added, call it `mirror`.

### Findings

#### 1. Small CLIs win. Tailscale's split between `up` and `set` is the clearest lesson: `up` resets every flag you leave out, `set` changes only what you name. A single partial-update `set` that also shows the config removes a whole class of mistakes.

**Evidence.** tailscale has about 35 top-level verbs (https://tailscale.com/kb/1080/cli). `tailscale set` "does not require the complete set of desired settings. Only settings explicitly mentioned will be set", while `up` applies OS defaults for flags you leave out and warns when you forget earlier ones (https://tailscale.com/kb/1241/tailscale-up). zerotier-cli has 9 verbs, each with a `-j` JSON switch (https://docs.zerotier.com/cli). wg has 9 subcommands, and `wg show <if> dump` gives tab-separated output for scripts (https://git.zx2c4.com/wireguard-tools/about/src/man/wg.8). wg-quick has 4 (up, down, save, strip). Nebula needs 7 manual steps and hand-written YAML for a first network (https://raw.githubusercontent.com/slackhq/nebula/master/README.md). clig.dev: "Don't have ambiguous or similarly-named commands", no catch-all subcommand, no arbitrary abbreviations (https://clig.dev/).

**Implication.** Adopt 8 verbs: install [token], invite, revoke, status, set, log, update, uninstall. Fold `init` into the first `invite`, `join` into `install <token>`, and `mode` into `set mode=`. `version` becomes `--version` plus the status header. Hide `daemon`. Rename `remove` to `revoke` so it cannot be confused with `uninstall`.

#### 2. Agents need one machine contract: stable JSON, stable error codes, few exit codes, no prompts, idempotent commands. wirebond already has that envelope in its socket protocol and can print it as is.

**Evidence.** clig.dev: "Never require a prompt", only prompt when stdin is a TTY, "operations should be idempotent where possible", machine output on stdout and messages on stderr (https://clig.dev/). gh uses exit codes 0, 1, 2 (cancelled) and 4 (auth needed) (https://cli.github.com/manual/gh_help_exit-codes), and GH_PROMPT_DISABLED turns prompting off (https://cli.github.com/manual/gh_help_environment). Tailscale's `status --json` carries "WARNING: format subject to change" (https://tailscale.com/kb/1080/cli). Practitioner guides for agent CLIs agree on JSON errors with a code and a hint, and on error codes being API contracts that never get renamed. They disagree on exit-code numbers (https://blog.arcjet.com/designing-a-cli-for-ai-agents/, https://dev.to/tumf/agentic-cli-design-7-principles-for-designing-cli-as-a-protocol-for-ai-agents-2c10). These are community conventions, not standards.

**Implication.** Adopt a global `--json` that prints the daemon's reply unchanged: `{ok,data}` or `{ok:false,error:{code,message,hint}}`. Exit 0 on success, 1 on failure (the reason is in error.code), 2 on bad usage. No prompts anywhere. Every mutating command is idempotent and returns `changed:true|false`. `status` carries `schema:1` and changes only by adding fields. Measure it with a test that runs every command twice and checks for the same result and exit code.

#### 3. Read-only commands should not need sudo. A password prompt stops an agent from diagnosing anything. Tailscale lets any local user read status and limits changes. Its 2026 advisory shows that letting a non-root user change things is where the risk is.

**Evidence.** tailscale status works without root; changes need root or the configured operator (https://www.ssdnodes.com/learn/tailscale-set-operator-run-without-sudo). Advisory TS-2026-005: a non-root operator could write Serve configuration through the LocalAPI and get privileged sockets proxied (https://cve.circl.lu/vuln/ts-2026-005). Peer credentials are available from x/sys/unix (SO_PEERCRED on Linux, LOCAL_PEERCRED on macOS). Windows AF_UNIX has SIO_AF_UNIX_GETPEERPID (https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Networking/WinSock/constant.SIO_AF_UNIX_GETPEERPID.html).

**Implication.** Adopt: socket mode 0666. The daemon reads the peer's uid and serves status, log and `set` without arguments to anyone, and every change only to uid 0. On Windows, keep requiring elevation until checking the peer's PID and admin token is worth its code. No operator concept.

#### 4. The install line in SPEC 5.1 is wrong for most machines: it hardcodes arm64. Go's default GOARM=7 for cross-compiled builds also breaks armv6 Raspberry Pis (Zero and 1).

**Evidence.** SPEC.md 5.1: `wirebond-$(uname -s | tr A-Z a-z)-arm64` gives Intel Macs and amd64 Linux the wrong file. Go's docs: when building a cross-compiler, "the default value is 7" for GOARM (golang/go src/cmd/go/alldocs.go, GOARM section). `uname -m` prints arm64 (Apple Silicon), x86_64, aarch64, armv7l and armv6l (Pi Zero/1); this is common knowledge, not checked against a source here. A static Go arm64 binary runs on a 64-bit kernel even with 32-bit userland, because it has no libc.

**Implication.** Name the assets after what `uname -sm` prints (wirebond-Darwin-arm64, Darwin-x86_64, Linux-x86_64, Linux-aarch64, Linux-armv7l, Linux-armv6l) and Windows-AMD64.exe and Windows-ARM64.exe. Build 32-bit ARM with GOARM=6 and publish the same file under both armv6l and armv7l. Then one line covers every Unix: `curl -fsSLo wirebond https://github.com/TristanCopley/wirebond/releases/latest/download/wirebond-$(uname -sm|tr ' ' -) && chmod +x wirebond && sudo ./wirebond install <token>`. On Windows: `...wirebond-Windows-$env:PROCESSOR_ARCHITECTURE.exe`.

#### 5. Checking for updates through api.github.com is fragile and unnecessary. The redirect from github.com's latest-release page gives the tag with no API call and no JSON.

**Evidence.** GitHub REST API: unauthenticated requests are limited to 60 per hour and "associated with the originating IP address" (https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api). Hotels and carrier NAT (seen on the owner's iPhone, CONTEXT.md) share IPs. I checked this live on 2026-11-05: `HEAD github.com/schollz/croc/releases/latest` returns 302 to `/releases/tag/v11.6.4`, and `/releases/latest/download/<asset>` returns 302 to `/releases/download/v11.6.4/<asset>`.

**Implication.** Replace SPEC 5.3 step 1. Send one HEAD request to `https://github.com/TristanCopley/wirebond/releases/latest` without following the redirect, and parse the tag from Location. Then fetch the asset and SHA256SUMS from `/releases/download/<tag>/`, never from `/latest/` twice, because a release published between two `latest` requests would mix an asset and a checksum file from different releases. Drop encoding/json from the update path. Add random jitter to the daily check.

#### 6. The Windows restart after an update, as written in SPEC 5.3, will not happen. Windows runs a service's recovery actions only when the process ends without reporting SERVICE_STOPPED. A Go `svc` handler that stops cleanly with exit code 3 counts as a normal stop.

**Evidence.** Microsoft: failure actions are queued "only if the service terminates without reporting a status of SERVICE_STOPPED" unless fFailureActionsOnNonCrashFailures is TRUE, in which case a stop with a nonzero exit code also counts (https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag). On the Nth failure the manager runs action N-1, "If N is greater than cActions, the service controller repeats the last action", and the count resets after dwResetPeriod (https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsa). x/sys exposes `SetRecoveryActionsOnNonCrashFailures` (https://pkg.go.dev/golang.org/x/sys/windows/svc/mgr). launchd by default does not respawn a job more than once every 10 s; that matters only for a job that ran less than 10 s (https://opensource.apple.com/source/launchd/launchd-442.21/man/launchd.plist.5.auto.html).

**Implication.** On Windows, call SetRecoveryActionsOnNonCrashFailures(true) and set three 1-second restart actions with a one-day reset period. Or have the updating daemon call os.Exit(3) without reporting stopped; do both. On macOS and Linux, apply an update with syscall.Exec of the new file: the same process, so it depends on no service manager and has no throttle. Add a Windows integration test that updates and checks the service is running again within 5 s.

#### 7. Self-replacement must rename, never overwrite, and should smoke-test before swapping. Renaming a running executable works on every OS; deleting or overwriting it does not on Windows, and overwriting in place gets the binary killed on macOS.

**Evidence.** In minio/selfupdate's apply.go, Apply writes `.<name>.new`, removes any old backup, renames target to `.old`, renames `.new` into place and renames back on failure. Its comments say Windows "can't remove the .old file because the process is still running" and that renames fail if the destination exists, so it hides `.old` instead (https://raw.githubusercontent.com/minio/selfupdate/master/apply.go). self_replace: a running Windows program can be renamed but not unlinked (https://docs.rs/self-replace/1). macOS caches the signature check per vnode, so a signed binary changed in place gets SIGKILL; a rename gives a new inode (reports at https://github.com/rust-lang/cargo/issues/8913 and https://github.com/eval-exec/neomacs/pull/91; my inference, not Apple documentation). The Go linker ad-hoc signs darwin/arm64 output itself, so cross-compiled Mac binaries run (https://github.com/golang/go/issues/42684). Tailscale's Windows updater copies its binary and re-executes it (https://pkg.go.dev/tailscale.com@v1.94.0/clientupdate).

**Implication.** Update flow: write `<exe>.new` with mode 0755 and fsync it. Run `<exe>.new version --json` and require the expected tag; this catches a wrong architecture, a truncated file, antivirus quarantine and a code-signing kill before anything changes. Then rename the old file to `.old` and `.new` into place. Delete `.old` only after the new daemon has run 60 s. Count starts in config.json and roll back to `.old` after 3 crashes in that window. Never open the installed executable for writing.

#### 8. Updating in the middle of a session hurts a gamer. A swap rebuilds the TUN interface, the control connections and the keys, so a daily automatic update can drop an Overwatch match or a Moonlight stream.

**Evidence.** This is design reasoning, not measured. SPEC 5.3 checks daily and updates by itself. SPEC 6.4: new keys arrive only with a new control connection, so a restart means TUN recreate plus a TLS reconnect and re-keying, plausibly 1–3 s of outage (not measured). Tailscale needs `--yes` to update without a prompt and lets the user pick a track (https://tailscale.com/kb/1080/cli).

**Implication.** Download and verify automatically, but apply only after 10 minutes with no data from any peer, or right away on `wirebond update`. Show `update: vX.Y.Z ready, applies when idle` in status. Keep peers compatible across versions within one major version, since devices update at different times; `hello` carries the version and only a different major version is refused.

#### 9. Go binary sizes are dominated by crypto/tls and net/http, which wirebond needs for control and updates. Since Go 1.24, FIPS self-test code added about 40% to crypto. A realistic wirebond lands at 4.5–6 MB stripped, about a quarter of Tailscale.

**Evidence.** golang/go#81132, built with CGO_ENABLED=0 -trimpath -ldflags='-s -w' on linux/amd64. Go 1.27.0: empty program 1,216,672 bytes, crypto/tls 2,699,424, crypto/x509 2,474,144, net/http 3,539,104. From go1.23.2 to go1.24.13, crypto/tls grew 42.65% (cause FIPS 140-3 init self-tests). A real TLS handshake program grew only about 11%, from 3.57 to 3.98 MB (https://github.com/golang/go/issues/81132). Tailscale's combined binary is 23 MiB, 18 MB stripped, 4.5 MB with UPX (https://tailscale.com/kb/1207/small-tailscale). Since Go 1.22, `-s` also drops the debug info that `-w` drops (linker docs; https://go.googlesource.com/go/+/refs/heads/release-branch.go1.27/src/cmd/link/doc.go). The wirebond estimate of 4.5–6 MB is mine, unbuilt, because Go is not on this machine.

**Implication.** Measure every release and fail CI above a budget of 7 MB for linux/amd64, printing `go tool nm -size -sort size | head -30` when it fails. Build flags: `CGO_ENABLED=0 go build -trimpath -tags nethttpomithttp2 -ldflags='-s -buildid= -X main.version=<tag>'`. The tag still exists on Go master, along with a client-only variant; a 2019 measurement put the saving at about 0.6 MB (https://golang.org/issue/35082). GitHub downloads work over HTTP/1.1.

#### 10. UPX would cut the size by about 4x, but it costs in exactly the places wirebond cannot afford: Windows Defender detections, W^X memory protection, startup RAM and macOS signing.

**Evidence.** Tailscale: packers "can reduce a binary's security", need W^X disabled or an executable temporary directory, and antivirus may flag them (https://tailscale.com/kb/1207/small-tailscale). A user-contributed Microsoft Q&A answer says packed files stay suspicious until they build reputation (https://learn.microsoft.com/en-us/answers/questions/5916419/is-there-risk-to-using-binary-compression). A Defender Trojan:Win32/Bearfoos.B!ml quarantine was addressed by dropping UPX (https://github.com/microsoft/apm/issues/487). UPX decompresses into RAM at start (https://oneuptime.com/blog/post/2026-01-07-go-reduce-binary-size/view).

**Implication.** Avoid UPX. A daemon that runs as SYSTEM, carries a kernel driver (Wintun) and replaces itself is the profile antivirus watches most closely. A 5 MB download needs no compression; GitHub serves assets as they are.

#### 11. Dependencies drive size and complexity. Common self-update and CLI libraries pull in far more than wirebond needs, and SPEC 4.2's dependency list is already incomplete.

**Evidence.** rhysd/go-github-selfupdate's go.mod has 12 modules, including google/go-github, x/oauth2, google.golang.org/appengine and ulikunitz/xz (https://raw.githubusercontent.com/rhysd/go-github-selfupdate/master/go.mod). minio/selfupdate is a fork of inconshreveable/go-update with minisign verification (https://github.com/minio/selfupdate). Calling reflect Method or MethodByName with a non-constant name makes the linker keep every exported method of reachable types: 118% size in one example and 100+ MB in GORM; Go 1.22 exempts only constant names (https://github.com/golang/go/issues/62257, https://appliedgo.net/spotlight/reflection-binary-size/). wireguard-go's go.mod requires x/crypto, x/net, x/sys, wintun and gvisor (https://git.zx2c4.com/wireguard-go/plain/go.mod). tun/offload_linux.go imports golang.zx2c4.com/wireguard/conn (https://git.zx2c4.com/wireguard-go/plain/tun/offload_linux.go), and conn uses x/net/ipv4 and ipv6. gvisor appears only in the module graph, not in the binary.

**Implication.** Write the update swap yourself (about 60 lines) and dispatch commands with a switch on os.Args plus the standard `flag` package. No cobra, no text/template, no html/template. Correct SPEC 4.2 to list x/net (pulled in through wireguard/tun) and add a CI check that the set of linked modules (`go version -m`) equals an allow-list.

#### 12. Adding a device should be one copy and paste. The inviting device knows everything the new device needs, so `invite` can print the complete command. The token is a one-time secret on the command line, which croc treated as a CVE, but here the risk is small.

**Evidence.** croc puts the code phrase in the CROC_SECRET environment variable on Linux and macOS "to avoid leaking the secret via the process name", citing CVE-2023-43621 (https://github.com/schollz/croc). The wirebond token (SPEC 6.2) is single-use and expires in 24 h, and it travels inside argv of a root process for about 1 s. SPEC's token carries a 32-byte fingerprint, the addresses, a 32-byte secret and an expiry, about 150 characters. 16 bytes of fingerprint and 16 bytes of secret keep 128-bit security (forging a CA would need a second preimage, not a collision) and cut it to about 60 characters (my analysis).

**Implication.** `wirebond invite` prints, for macOS/Linux and for Windows, the full download-and-install line ending in `install wb1:<token>`, plus the line for an already-installed device. If no network exists, `invite` creates one, so `init` goes away. Use a compact binary token of about 60 characters. Accept the argv exposure, and document that a token sitting unused in shell history expires in 24 h.

#### 13. Phones can contribute today with nothing installed on them: a phone's hotspot over USB or Wi-Fi is already a link to the laptop. Making a phone an overlay member needs a NetworkExtension (iOS) or VpnService (Android) app under tight memory limits. Go fits, but only just.

**Evidence.** Reported iOS packet-tunnel memory limits are 15 MiB on iOS 14.8 and 17, and 50 MiB on iOS 15–16; reports conflict (https://developer.apple.com/forums/thread/73148?page=2). Tailscale's Go NetworkExtension used 10–12 MB of an approximately 15 MB budget, and they cut relocations so dirty `__DATA_CONST` fell from 1568 KB to 708 KB (https://tailscale.com/blog/go-linker). wireguard-go has iOS-specific queue sizes (https://git.zx2c4.com/wireguard-go/tree/device/queueconstants_ios.go). CONTEXT.md: the iPhone hotspot gives the Mac 172.20.10.0/28 over USB or Wi-Fi.

**Implication.** For now, the phone story is that its hotspot is a lane. `status` should label tether links by what they are, e.g. `iPhone USB`, using the 172.20.10.0/28 range and the interface type, so a non-expert sees the phone at work. If a phone app is built later, keep the data plane free of heap-heavy paths and set GOMEMLIMIT to about 10 MiB, which needs a measured memory budget in CI.

#### 14. "primary" and "aggregate" describe the mechanism, not the outcome. The one commercial bonding product names its modes by effect, and its choice of "Speed" shows the trap: gamers read speed as latency.

**Evidence.** Speedify has Speed (the default, which bonds capacity) and Redundant ("runs at the speed of your fastest single connection" while sending on all connections), plus an Enhance Streaming add-on that prioritizes real-time traffic (https://support.speedify.com/article/870-bonding-mode, https://support.speedify.com/article/242-redundant-mode). The owner's own words are "stability mode" and "throughput mode", and CONTEXT.md calls aggregate the "extra capacity" toggle.

**Implication.** Rename the modes to `stable` (default: one best path with the others as backup) and `combine` (adds path capacities and costs some latency). Set them with `wirebond set mode=combine`. If an always-duplicate mode for gaming is added, name it `mirror`. Use these words in `status` too, e.g. `mode stable · primary lane iPhone USB`.

### Proposals

- Replace SPEC §11 with 8 verbs: `install [token]`, `invite`, `revoke <device>`, `status`, `set [key=value ...]`, `log`, `update [--check]`, `uninstall [--purge]`. Hide `daemon`, make version available as `--version` and in the status header, remove `init`, `join` and `mode`, and rename `remove` to `revoke`.
- Make `invite` create the network when this device has none, and have it print the full one-line download-and-install command for macOS/Linux and for Windows, ending in `install wb1:<token>`.
- Make `install` idempotent with an optional token: it installs or upgrades, then joins. With the same token again or an existing membership it reports `changed:false`.
- Make `set` with no arguments print the config, and with `key=value` pairs (mode, endpoint, control, port, autoupdate) apply them all at once. Keys you leave out never change.
- Rename the modes to `stable` (default) and `combine` everywhere: the spec, config.json and status.
- Add a global `--json` that prints the daemon's socket reply unchanged: `{"ok":true,"data":...}` or `{"ok":false,"error":{"code","message","hint"}}`, with a stable list of error codes in the spec.
- Define exit codes as 0 success, 1 failure (reason in error.code) and 2 usage error, and state that wirebond never prompts.
- Give `status --json` a `schema: 1` field that only ever gains fields, and a `problems` array of `{code, hint}` (e.g. `fallback_only`, `rp_filter_strict`, `firewall_blocked`, `update_ready`).
- Make the CLI socket mode 0666 on macOS and Linux. The daemon checks the peer's credentials, serves status, log and a bare `set` to any user, and allows changes only to uid 0.
- Rename release assets to what `uname -sm` and `$env:PROCESSOR_ARCHITECTURE` print (Darwin-arm64, Darwin-x86_64, Linux-x86_64, Linux-aarch64, Linux-armv7l, Linux-armv6l, Windows-AMD64.exe, Windows-ARM64.exe), and build 32-bit ARM with GOARM=6.
- Replace the api.github.com check with a HEAD to `github.com/TristanCopley/wirebond/releases/latest`, parse the tag from Location, and download the asset and SHA256SUMS from `/releases/download/<tag>/`.
- Before swapping, run `<exe>.new version --json` and require the expected tag. Never write into the installed executable.
- Keep `.old` until the new daemon has run 60 s, and roll back automatically after 3 crashes in that window, using a start counter in config.json.
- On macOS and Linux, apply an update with syscall.Exec of the new file. On Windows, set SetRecoveryActionsOnNonCrashFailures(true) with three 1-second restart actions and a one-day reset period, and exit without reporting SERVICE_STOPPED.
- Download updates automatically, but apply them only after 10 minutes with no peer data, or right away on `wirebond update`.
- Build with `CGO_ENABLED=0 -trimpath -tags nethttpomithttp2 -ldflags='-s -buildid= -X main.version=<tag>'`, never use UPX, and fail CI when linux/amd64 exceeds 7 MB.
- Correct SPEC §4.2 to include x/net (pulled in through wireguard/tun's conn package), and add a CI allow-list check on `go version -m`. Forbid cobra, text/template and html/template.
- Shorten the invite token to about 60 characters: a 16-byte fingerprint prefix, a 16-byte secret, packed control address and expiry.
- Have `status` label phone tether links by name (e.g. `iPhone USB`), so the phone's part is visible without a phone app.


## Competitive landscape

### Summary

1. No shipping product does what wirebond's spec describes: bonding between two of the owner's own devices, with no server in the middle, on desktop operating systems, alongside Tailscale. Speedify, Peplink SpeedFusion, Bondix, OpenMPTCProuter and the newer open-source mqvpn all end the bond at a server, which reorders the packets and sends them out to the internet. Tailscale has had an open bonding request (#2256) since June 2021. Its staff said in 2021 that they expect to support it, but none has shipped. Peer Relays went GA in February 2026 and are only a fallback tried after a direct path fails; they never use paths at the same time. Mullvad and Cloudflare WARP use one path only.

2. For Moonlight from Xanth, the home PC already plays the role a bonding server plays elsewhere. So wirebond's "no server in the middle" is a real, strict win on latency, cost and privacy. For native internet games on the laptop, every bonder adds a detour, and wirebond would detour through Los Angeles. That is physics, and it is out of scope.

3. Every serious competitor converges on two techniques. One is copying latency-critical packets onto a second link: Speedify Redundant and Enhance Streaming, Peplink WAN Smoothing at 2x to 4x, Bondix duplication presets, mqvpn reinjection. The other is reordering at the receiver for aggregation. This supports wirebond's design of copying urgent packets and hedging. wirebond is cheaper because it copies only small urgent flows. Peplink's Smoothing doubles all data.

4. Measured aggregation tops out at about 80 to 85% of the summed link capacity in independent or reproducible tests:
   - MPTCP over Starlink plus cellular: 81 to 84% use, 30 to 66% above the better single path, but only after raising buffers above 10x the bandwidth-delay product. With default buffers the gain was marginal and the throughput sometimes collapsed to zero.
   - mqvpn: 84% (self-reported, emulated).
   - Peplink's own overhead model: 81% for a typical packet mix.

   Speedify's "95%" claim is unsupported. A4's 80% target is the right frontier; do not promise more.

5. Failover numbers are scarce:
   - Speedify: about 1 s, measured independently, and TCP connections survived.
   - Peplink and Bondix publish no figures. Bondix and Digi claim "sub-second" or "no wait".
   - wirebond's spec: a lane is suspect after max(150 ms, 2×RTT + 4×deviation), which is about 280 ms on the hotspot. Copies of urgent packets mean zero urgent loss.

   With these numbers wirebond can be measurably best. Publish them, because no one else does.

6. Physics wirebond cannot beat:
   - Base latency is at best the minimum over its lanes. Chicago to LA in fiber is a floor of about 28 ms round trip. The owner measured 64 ms on hotel Wi-Fi and about 95 ms on cellular.
   - A lane cannot carry more than its shaper allows (38 Mbps at the hotel).
   - Aggregating paths adds the delay difference between them (skew).
   - Starlink's 15-second reconfigurations (at :12, :27, :42 and :57 past each minute) and its tail of 11 to 16 times the median can only be hidden by copying onto another link.
   - Both ends behind carrier-grade NAT need a relay.

7. In the hotel case, video on the cellular lane costs about 15 ms more one-way than on hotel Wi-Fi. Fitting the stream under the hotel cap (for example HEVC at 30 Mbps) is the low-latency choice. wirebond can tell the user this; no competitor does.

8. Phones: Speedify bonds Wi-Fi and cellular on iOS and Android. mqvpn has an experimental Android client and an iOS client in development. Speedify's Pair & Share also pools other phones' cellular over the LAN. Phones are wirebond's largest gap against the owner's "all devices" goal. The fix is an app (NetworkExtension on iOS, VpnService on Android) built around the existing Go data plane.

9. Footprint and complexity: wirebond is one static Go binary, free, with no server.
   - Speedify's self-hosted server needs 2 cores, 4 GB of RAM, Docker and a static IP, plus a paid add-on (promo $28 a month).
   - Peplink needs its hardware plus a PrimeCare subscription, and its data allowances throttle to 10 Mbit/s when used up.
   - Bondix needs a Teltonika or OpenWrt router, and its licenses are capped at 20 to 500 Mbps.
   - OpenMPTCProuter needs an OpenWrt router plus a VPS running kernel 6.18.
   - mqvpn is C plus XQUIC plus BoringSSL.

   Multipath QUIC is still an Internet-Draft (draft-21, no RFC), so adopting it adds weight without settled interoperability.

10. One immediate robustness and security item: since Go 1.24, Linux listeners open as Multipath TCP by default. On 2026-10-30 the Go team posted about CVE-2026-98432, a Linux TCP use-after-free whose public remote exploit needs an MPTCP listener. wirebond's TCP control port 7849 on Linux would be such a listener unless it opts out.

Unverified: the Speedify, ExitLag and Bondix performance claims, all prices except Peplink's published allowances, the 50 MB iOS extension limit, and the CVE details beyond the Go team's oss-security post.

### Findings

#### 1. Comparison table: architecture, modes, measured numbers, price, footprint, platforms and complaints for each competitor

**Evidence.** Columns: product | server in the middle? | modes | measured or claimed numbers | price | footprint | clients (phones?) | main complaints.

Speedify | Yes. Its cloud servers in 50+ cities reorder packets and send them to the internet; a self-hosted server runs in Docker (2 cores, 4 GB RAM, 20 GB disk, static IP) | Speed (per-packet bonding); Redundant (every packet on every link); Enhance Streaming (switches stream traffic between Speed and Redundant; on by default). Transports: UDP, TCP, several TCP sockets, HTTPS | Independent (wirelessmoves, Raspberry Pi 4, 3 links): failover under 1 s and TCP connections survived; bonded downlink peaks about 120 Mbps but drops to 10–20 Mbps for seconds; uplink 2×30 → about 60 Mbps; 80% of one Pi 4 core at 120 Mbps. Vendor claim: 'up to 95%' of combined capacity (unverified) | About $5–15 a month from third-party listings (unverified); self-hosted add-on promo $28 a month Personal, $84 Enterprise | Windows 10, macOS 11.5, Linux, iOS 17, Android 9, OpenWrt | Detour through its servers; subscription; reviewer says it logs IP addresses; downlink dips; CPU use.

Peplink SpeedFusion / SpeedFusion Connect | Yes. Peplink-hosted PoP, a FusionHub VM, or a second Peplink device | Bonding; WAN Smoothing (Normal at most 2× data, Medium 3×, High 4×); Hot Failover; FEC (Low 13.3%, High 26.7%, Adaptive 6.7–20%) | Whitepaper: overhead 19% of a typical packet mix (IMIX), 5% at 1,500 B, 200% at 40 B; recommends links within 50% of each other's bandwidth and 150 ms of each other's latency. No failover time published; a reseller notes switchover starts only after line monitoring detects the failure | SFC about 3–4 ¢/GB (reseller list); bundled 500 GB a year (B One) or 1 TB (Balance 310), then throttled to 10 Mbit/s; bonding needs PrimeCare | Peplink hardware plus a VM | Lock-in to hardware and subscription; data caps; Smoothing doubles metered data.

Bondix S.A.NE | Yes. Bondix server as a VM image (OVA), self-hosted or hosted | Tunnel presets: bonding, bondingDistribute (wiki warns it adds jitter when fiber is mixed with poor 4G), duplication, duplication2 (up to 3 links), failover, loadbalancer (experimental). Channel presets include a TCP mode | No independent numbers; vendor and Digi claim 'sub-second' or 'no wait' failover | Licenses capped at 20, 100, 200 or 500 Mbps, 4–6 links; prices only in the shop | Teltonika RUT/RUTX, AnyRover and OpenWrt routers only; no desktop or phone client | Router-bound; opaque internals.

OpenMPTCProuter | Yes. A VPS running Debian 13, kernel 6.18 | MPTCP with BPF schedulers (default, rr, redundant, BLEST, ECF, …); MQVPN (multipath QUIC, the default VPN since v0.64), Glorytun, MLVPN, Shadowsocks, XRay | No official benchmarks. Issues: UDP not aggregated by default or 'not really good' (#3190, #3191); glorytun-tcp ping rose from 50 ms to 3,000 ms (#213) | Free, plus the VPS | OpenWrt routers, Raspberry Pi 5, x86; no phones | Poor UDP and gaming behavior; heavy stack.

mqvpn | Yes. The server is Linux only and acts as a NAT exit | Schedulers: minrtt, wlb, reinjection (copying packets on a deadline), reorder buffer | Self-reported, emulated: 319 of 380 Mbps (84%); SRT loss 31.5% → 0.06%; 'zero downtime' failover with no figure in ms | Free, Apache-2.0 | C with XQUIC and BoringSSL | Linux, Windows, macOS arm64, Android (experimental), iOS (in development) | Young project (138 stars).

Tailscale | No server for direct paths; DERP or peer relays only as fallback | One path at a time: direct, then peer relay, then DERP. No multipath; issue #2256 has been open since 2021 | Peer relays: vendor says throughput 'nearing' a direct connection, no figures | Free personal plan; 2 peer relays free | All OSes including iOS and Android | No bonding.

Mullvad | Yes (VPN server; multihop adds another) | Single path; obfuscation; DAITA | Only anecdotal latency costs for DAITA and multihop | Flat monthly price (not checked here) | All OSes | Not a bonder.

Cloudflare WARP | Yes (Cloudflare edge) | Single path; MASQUE over HTTP/3 by default since Dec 2024, or WireGuard | Only vendor claims ('10× throughput, 3× lower latency' with no named baseline); one anecdote of +56 ms | Free, or WARP+ | All OSes | Not a bonder; detour.

ExitLag | Yes (relays near game servers) | Route optimization; 'multipath routes'; 'Multi Internet' uses local links together | No independent tests or packet captures found | About $5–10 a month (resellers disagree) | Windows, Android, iOS; macOS not found | Closed; claims unverifiable; helps only for routes to game servers.

Starlink + cellular | Done through one of the above | — | MPTCP: +30–66% over the better path, 81–84% use, only with buffers above 10× BDP (Hu et al.) | — | — | — | See the Starlink finding.

wirebond (spec) | No: the two devices are the ends | Primary mode with urgent copies and hedging; aggregate mode with reordering | Spec targets: urgent loss 0, bulk gap ≤ 300 ms, aggregate ≥ 80% | Free | One static Go binary | macOS, Linux, Windows; no phones yet | Not yet built.

**Implication.** wirebond's distinct position is device-to-device bonding on desktop operating systems, with no server, no subscription, and no second VPN under Tailscale. Keep that position and do not drift toward a hosted service.

#### 2. Tailscale still has no multipath or bonding, and its Peer Relays (GA February 2026) are only a fallback tried after the direct path, never used at the same time

**Evidence.** github.com/tailscale/tailscale/issues/2256 'Support multiple links bonding' was opened 2021-06-26 and is still open with 21 comments. DentonGentry (Tailscale) wrote on 2021-08-19: 'I do expect to support multiple wireguard connections ... but right now we only use one at a time.' New requests #21143 (2026-09-06) and #18221 (2025-12-16) are open. Peer relays try direct first, then peer relay, then DERP; they need Tailscale 1.86 or later and one reachable UDP port; two are free (https://tailscale.com/blog/peer-relays-beta, https://tailscale.com/kb/1591/peer-relays).

**Implication.** Tailscale is unlikely to make wirebond obsolete soon. A peer relay on the Pi or on Xanth gives wirebond's fallback lane a better relay than DERP for both-CGNAT cases, at no code cost (an owner step).

#### 3. For Moonlight from Xanth, the home PC is the 'server in the middle', so wirebond's device-to-device design has zero detour. Every commercial bonder adds a hop for this traffic.

**Evidence.** Speedify, Peplink and Bondix reorder at a server and send traffic on from there (support.speedify.com/article/273; Peplink whitepaper ch. 2; wiki.bondix.dev/wiki/Architecture). Ascend (Peplink reseller) notes each detour adds round-trip time and recommends a nearby FusionHub. CONTEXT.md: Speedify was rejected for detours, subscription and a second VPN under Tailscale.

**Implication.** Adopt the zero-detour position as the main value claim. For native competitive games on the laptop (not streamed), any bonder must exit somewhere. wirebond would exit at home in LA, which adds the Chicago–LA distance (about 57 ms per CONTEXT.md). Keep exit routing out of scope and say so plainly.

#### 4. Copying latency-critical packets is the industry-standard stability technique. Vendors differ in how much they copy and what it costs.

**Evidence.** Speedify Redundant sends everything on every link; Enhance Streaming switches stream traffic between Speed and Redundant (support.speedify.com/article/692, /242). Peplink WAN Smoothing costs at most 2×, 3× or 4× the data; its whitepaper says Smoothing 'tends to work more reliably on its own' than FEC on volatile links. Bondix has duplication and duplication2 presets. mqvpn has deadline-based reinjection. Background: Vulimiri et al., 'Low latency via redundancy' (arXiv:1306.3707), find that copying reduces mean and tail latency while load is low.

**Implication.** Keep §9.1's per-flow urgent copies (small packets, almost free) and copying bulk only while hedging. Do not copy bulk video by default: at 60 Mbps a second copy is about 27 GB an hour on the hotspot (my arithmetic).

#### 5. Measured aggregation tops out at about 80–85% of summed capacity, needs large buffers, and degrades badly when the links differ greatly

**Evidence.** Hu et al., 'LEO Satellite vs. Cellular Networks: Exploring the Potential for Synergistic Integration' (https://par.nsf.gov/servlets/purl/10536713): MPTCP over Starlink plus AT&T or Verizon reached 81% and 84% use and +30% and +66% over the better path. With default buffers the gain was 'marginal' and throughput sometimes collapsed to 0, so buffers were raised above 10× the bandwidth-delay product. Peplink: 200+200 Mbps → 162 Mbps after 19% overhead on a typical packet mix; keep links within 50% bandwidth and 150 ms. mqvpn: 84% (self-reported). wirelessmoves: Speedify downlink dipped to 10–20 Mbps for seconds.

**Implication.** A4's ≥ 80% target matches the best evidence; do not chase Speedify's unverified 95%. Make sure the reorder buffer and socket buffers are sized from the bandwidth-delay product of the slowest lane, not fixed. Consider excluding a lane from aggregation when its OWD is more than about 150 ms worse or its rate is under 1/10 of the fastest (Peplink's guidance).

#### 6. Failover time is rarely published. The only independent figure (Speedify) is about 1 s. wirebond's spec detection is roughly 3–4 times faster, and urgent packets never wait for detection.

**Evidence.** wirelessmoves: 'Speedify reacted within a second' and iperf3 TCP survived (https://blog.wirelessmoves.com/?p=12440, Raspberry Pi 4). Peplink publishes no failover time; it switches after line monitoring detects the failure (ascend.de). Bondix and Digi claim 'sub-second' or 'no wait' with no method. wirebond SPEC §7.3: suspect after max(150 ms, 2×RTT + 4×dev), about 95 + 2×23 ≈ 280 ms on the hotspot (my arithmetic); urgent packets already travel on two lanes; A2 allows a bulk gap of at most 300 ms.

**Implication.** Measure and publish failover numbers (urgent loss, bulk gap, time to new primary) from netsim and the tc-netem integration tests. No competitor publishes these, so evidence is a cheap way to lead.

#### 7. Physics floor for the owner's case: a lane cannot do better than its own base delay and its shaper. Putting video on the high-capacity cellular lane costs about 15 ms one-way compared with hotel Wi-Fi.

**Evidence.** Chicago–LA great circle about 2,800 km; at about 200,000 km/s in fiber that is about 14 ms one-way, about 28 ms round trip (my arithmetic). CONTEXT.md measurements: Mac → Xanth direct over hotel Wi-Fi 64 ms round trip, over the hotspot about 95 ms; the hotel shaper is about 38 Mbps. Input packets, copied on both lanes, arrive at the lower one-way delay (about 32 ms). Video arrives at the primary lane's one-way delay: about 32 ms on hotel, about 47 ms on cellular.

**Implication.** The spec's 'lowest RTT first, promote only if congested' rule is right. wirebond should tell the user the highest rate the low-latency lane carried without congestion, so they can set Moonlight's bitrate to fit (for example 30 Mbps HEVC under the hotel's 38 Mbps) and keep video on the faster lane.

#### 8. Starlink has a predictable 15-second latency pattern and a heavy tail, so copying onto another link is the only cure

**Evidence.** Global reconfiguration at :12, :27, :42 and :57 past each minute (Tanveer, NITRD slides 2024: https://www.nitrd.gov/nitrdgroups/images/2024/JET-Hammas-Bin-Tanveer-08202024.pdf; Mohan et al., WWW 2024, arXiv:2310.09242). Boundary regions last about 140 ms at the start and about 75 ms at the end of each cycle; within the cycle, prediction p99 error is under 50 ms (Casparsen et al., arXiv:2601.08439). Worst-case latency is 11–16× the median (arXiv:2310.11835). Speedify Starlink Index (vendor data): 33–46 minutes offline per dish per day.

**Implication.** Add a Starlink netsim scenario. wirebond's per-flow urgent copies already cover it. A cheap generic improvement: when a lane shows periodic OWD spikes, copy urgent and bulk packets during the predicted windows. Treat this as a later option, measured first.

#### 9. Phones are wirebond's largest gap against both competitors and the owner's 'all devices' goal. Competitors prove it is feasible on iOS and Android.

**Evidence.** Speedify bonds Wi-Fi plus cellular on iOS 17 and Android 9 (support.speedify.com/article/960). Pair & Share pools phones' cellular over a shared LAN, at most 10 Mbps per phone according to Speedify (support.speedify.com/article/895). mqvpn: Android 8+ experimental, iOS 15 in development. iOS packet-tunnel extensions have a hard memory limit: 15 MB historically, reportedly 50 MB since iOS 15 (openradar 27660401; unverified). Tailscale caps wireguard-go memory on iOS (wgengine/mem_ios.go). SPEC §15 already notes that gomobile can embed the data plane.

**Implication.** Plan a V4 phone app: iOS NetworkExtension and Android VpnService wrapping internal/wire, lane and sched, with lanes bound per interface type. Design hot-path buffers to fit a 15 MB budget now, so the phone port needs no redesign. Until then, USB tethering is the zero-code way to use the phone.

#### 10. Copying bulk packets and FEC cost metered data quickly. Moonlight and Sunshine already carry 20% FEC per frame.

**Evidence.** Peplink Smoothing Normal doubles data (2.7 → 5.4 GB an hour in Ascend's example); FEC costs 6.7–26.7%. Sunshine's fec_percentage default is 20 ('what GeForce Experience uses', per an Apollo fork commit; https://sunshinestream.readthedocs.io/en/v0.14.1/about/advanced_usage.html). Moonlight at 60 Mbps is about 27 GB an hour, so one copy on the hotspot doubles that (my arithmetic).

**Implication.** Do not add FEC to wirebond; Sunshine already provides it, and copying is the better tool on volatile links. Show the bytes copied per lane in status so the cost of hedging on a metered lane is visible, without adding a 'metered' flag.

#### 11. Go 1.24+ opens Linux TCP listeners as Multipath TCP by default. A Linux TCP use-after-free disclosed on 2026-10-30 has a public remote exploit that needs an MPTCP listener.

**Evidence.** Go godebug.md: since Go 1.24, multipathtcp=2 by default (MPTCP on for listeners). Go team on oss-security, 2026-10-30 (https://www.openwall.com/lists/oss-security/2026/10/30/1): the demonstrated exploit of CVE-2026-98432 used MPTCP listeners; mitigations are a patched kernel, GODEBUG=multipathtcp=0, 'godebug multipathtcp=0' in go.mod, or ListenConfig.SetMultipathTCP(false). Tailscale issue #22135 applied the same opt-out. CVE details beyond the Go post are unverified.

**Implication.** wirebond's control listener on TCP 7849 must turn MPTCP off explicitly. It needs no MPTCP, since multipath is wirebond's own job.

#### 12. Multipath QUIC and MASQUE are where open-source tools are heading, but the standard is not settled and the stacks are heavy

**Evidence.** draft-ietf-quic-multipath is at revision 21 with no RFC number (datatracker API). mqvpn is built on XQUIC, BoringSSL and MASQUE CONNECT-IP (RFC 9484). OpenMPTCProuter made MQVPN its default VPN in v0.64. Cloudflare WARP uses MASQUE by default.

**Implication.** Keep wirebond's own 10-byte header with ChaCha20-Poly1305 over UDP. It is smaller, has no external interoperability burden, and gives exact control over copying. Revisit only if multipath QUIC becomes an RFC and a small pure-Go implementation exists.

#### 13. Gaming 'optimizers' such as ExitLag solve a different problem (the route to a game server) and have no verifiable measurements

**Evidence.** ExitLag's own pages describe 'multipath routing' and 'Multi Internet'. No independent tests or packet captures were found. Platforms: Windows, Android, iOS; no macOS found. Price about $5–10 a month (resellers disagree). Hone, a competitor, advises A/B testing on the user's own route.

**Implication.** Do not compete on routes to game servers. For streamed gaming, wirebond's advantage is input copies plus a stable primary lane between the owner's own devices. Make that measurable in status: per-lane OWD, loss and copy wins.

#### 14. Ease of use: every competitor needs an account, a server, a router or a subscription. Agents need machine-readable status, and none of these products document one.

**Evidence.** Speedify: account, plus Docker and a static IP to self-host. Peplink: hardware and PrimeCare. Bondix: router and license. OpenMPTCProuter: router and VPS. mqvpn: Linux server and a pre-shared key; its control API is unauthenticated JSON over TCP and off by default. wirebond: install, then init and join, with a JSON CLI socket already in SPEC §11.

**Implication.** Expose the socket's JSON directly as 'wirebond status --json', with stable field names, so agents can check lanes, primary, copy wins and failover counters without screen-scraping.

### Proposals

- In §6.4, require the control listener to call net.ListenConfig.SetMultipathTCP(false) and add 'godebug multipathtcp=0' to go.mod, citing the Go team's 2026-10-30 oss-security advisory (CVE-2026-98432).
- In §6.5, add an owner step: run 'tailscale set --relay-server-port' on the Pi or Xanth (Tailscale 1.86 or later), so the fallback lane for two CGNAT ends goes through a peer relay rather than DERP.
- Add §12.1 scenario 'starlink': a 15-second cycle aligned to :12, :27, :42 and :57, with boundary spikes of 140 ms and 75 ms, a latency tail of 11–16× the median, and about 2.3% of each day offline, paired with a cellular lane; add acceptance: zero urgent loss and p99 urgent latency no more than the lower lane's OWD plus 10 ms.
- Add acceptance A9: publish the measured failover figures (urgent packets lost, longest bulk gap, time to a new primary) from netsim and the netem integration tests in the README, because the only independent competitor figure is Speedify's 'within a second'.
- In §11, make 'status --json' return the socket's JSON with stable field names (lanes, primary, rtt, owd, queueDelay, loss, capacity, sendRate, dupBytes, dupWins, failovers) for agents.
- In §11 status, show per lane the highest offered rate it carried without congestion ('fits ≤ 35 Mbps'), so the user can set Moonlight's bitrate to keep the lower-latency lane primary.
- In §11 status, show the bytes each lane sent as copies (urgent copies and hedging), so the data cost of a metered hotspot is visible without adding a 'metered' setting.
- In §9.2, size the reorder buffer and lane socket buffers from the slowest lane's bandwidth-delay product, at least 2× and up to 10× as the Starlink–cellular MPTCP study needed, rather than fixed values.
- In §9.2, exclude a lane from aggregation (urgent copies only) when its base OWD exceeds the fastest lane's by more than 150 ms or its capacity is below one tenth of the fastest lane's, following Peplink's measured guidance.
- In CONTEXT.md, record that wirebond adds no FEC because Sunshine already sends 20% FEC per frame and Peplink finds copying more reliable than FEC on volatile links.
- In CONTEXT.md's decisions table, add the comparison table and the zero-detour argument: for Moonlight, Xanth itself is the bonding end, so every server-based bonder adds a hop that wirebond does not.
- Promote 'Phones and tablets' in §15 to a planned V4 (iOS NetworkExtension, Android VpnService, gomobile wrapping internal/wire, lane and sched), and require now that hot-path buffers fit a 15 MB extension memory budget.
- In §15, keep exit routing out of scope with the stated physics: native internet games through a home exit pay the Chicago–LA detour (about 57 ms round trip), which no bonder avoids without a server near the game server.
- In CONTEXT.md, record that wirebond keeps its own UDP format rather than adopting multipath QUIC or MASQUE, because draft-ietf-quic-multipath is still a draft (revision 21, no RFC) and the available stacks (XQUIC with BoringSSL) are heavy C dependencies.
