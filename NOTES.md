# wirebond 2.0: notes

Where the 2.0 build stands, the decisions taken while building it that SPEC.md does not record,
and what the owner has to do. SPEC.md is the specification; notes/2.0-build.md fixes the build's
interfaces and who owns which file. The 1.x notes (notes/rust.md, notes/open.md and the Go-era
notes) are gone; their history stays in git.

## Status

| Part | State |
| --- | --- |
| Engine (`crates/core`) | SPEC §10–§15: both modes, the contribution statistics (first arrivals, rescues, reorder waits), every fragment of a datagram in one flow, later copies not opened (receive cost halved). 162 unit tests and the integration suites pass; one pair test (§14.1's known limitation) and the six vector tests are ignored with reasons. |
| Simulator (`crates/netsim`) | Real inner IP packets, the anchor as destination and as exit, IPv4 fragmentation. The PR seed set passes 10 of 11 (A4 is ignored, see Open); swarm 3 of 3; fragmented 1,392-byte video loses nothing in performance mode. The full set (`WIREBOND_LONG=1`, 20 seeds each, about 30 minutes; `release.yml` requires it and CI had never run it) passes 8, ignores A4 and fails `a5_bandwidth` and `a6_bandwidth`; see Open 4. Performance mode passes both. |
| Daemon, OS layer, CLI (`crates/wirebond`) | Roles and the bond lifecycle, control protocol v2, the TUN pump, capture and egress on Linux, macOS and Windows, the DNS relay (UDP and TCP), STUN and bottleneck groups, fail open, captive portals, sleep and wake, `pick`, the support role, the update schedule, the local API and remote views, every CLI verb, `install` among them. 122 tests pass on Linux, macOS and Windows. |
| Install (`install.sh`, `install.ps1`) | One-liners that download this machine's release asset, check it against `SHA256SUMS` and run `wirebond install` as an administrator. `wirebond` lands on PATH everywhere: `/usr/local/bin` on Linux, a `/usr/local/bin` link to the root-owned executable on macOS, the machine PATH on Windows; `wirebond uninstall` removes them. CI installs, checks and uninstalls for real on Linux (systemd), macOS (launchd) and Windows PowerShell 5.1 (SCM) on every push. They work once a release exists (see Open). |
| Load tool (`crates/load`) | Echo, paced UDP, 120 fps video frames, TCP bulk, CPU and RSS, wait, checks, tables. Standard library only; replaces the Go tool. |
| System harness (`tests/system/run.sh`) | With the real daemons: A7, A8, A9, A10 and the shaped hotel pass (0 of 1,200 frames lost; frame delay p99 14 ms against the mesh-lane baseline's 120 ms). A12 loses nothing at 100 Mbit/s but each daemon uses 33-42% of a core in a cloud container and 51-63% on CI's runners, against its 25% budget (Open 2). netem runs on CI only. |
| CI | Public runners since the repository went public. Every job passes: lint, the Linux tests and netsim's PR seeds, clippy on every release target, the tests on Windows and on macOS (one at a time, 79 s), the installers on all three, and `system.yml`'s namespace harness, with A12 held to 85% there (Open 2). Until 2.0.0-rc.1 that job could not fail: its harness step piped into `tee` under a shell without `pipefail`, so it was green while `run.sh` printed `failed: a12` on every run. The nightly run (the full acceptance set) fails, on the three tests of Open 4; it runs every suite now, where it used to stop at the first to fail. |

## Decisions

- **Delay is a round trip through an echo.** The load tool stamps each datagram and the echo returns
  it, so sender and receiver are one process and no clocks need to agree. Every delay percentile is
  a round trip, both directions of the bond loaded at the same rate.
- **Video as Moonlight sees it.** `load video` sends frames of bitrate ÷ fps bytes in 1,400-byte
  shards plus 20% Reed–Solomon parity (Sunshine's default, as netsim models it), each frame a burst
  at its frame time. A frame is decodable once as many shards as it has data shards came back; a
  shard of a later frame purges every earlier incomplete frame and a shard of a passed frame is
  late, as in Moonlight's RtpVideoQueue. Frame delay is the frame's send to its last needed shard.
- **The echo counts by source address,** which is how the harness tells NAT at the anchor (the
  anchor's uplink address), local delivery (the client's bond address) and forged sources apart.
- **Checks travel with the results.** Each measurement records its `--check` verdicts on its JSON
  line; `load table` prints and re-evaluates them, and the exit status is the verdict.
- **The harness's baseline is the bond on its mesh lane alone** (direct lanes dropped at the
  anchor), the same daemons and traffic: SPEC P1's baseline, and netsim's.
- **The mesh rides the hotel,** so the harness shapes the mesh bridge's ports as it shapes the hotel.
- **Daemons run as installed:** in each namespace, `/var/lib`, `/var/run` and `/usr/local/bin` are
  bound over from the run directory in a private mount namespace, and `/etc/resolv.conf` names the
  server's echo, the anchor's upstream resolver. Nothing of the machine's is touched.
- **macOS CI aliases only the loopback addresses the tests name** (about 110: `ip(net, host)`, `Ipv4Addr::new(127, …)`, `"127.x.y.z"` and `bond_end_to_end`'s nets). macOS assigns only 127.0.0.1 on lo0, so the daemon tests need aliases, but the 3,900 that an earlier step added made every link scan (getifaddrs, netstat) crawl: the suite ran past 25 minutes, looking hung, where it now takes 79 s. A test that binds a new address must be named the same way, or the step's pipeline taught it.
- **The installers do the least:** download, check, `wirebond install`. Everything else (the service, the PATH entry or link, the firewall rule, `wintun.dll`) is the executable's, so a manual download installs the same way and `uninstall` undoes all of it.
- **The harness works without IPv6 in the kernel,** skipping the IPv6 halves of its checks and
  saying so; CI's runners have IPv6, so there they run.

## Open

Decisions for the owner:

1. **Bandwidth under a sudden full-rate load.** Lanes that share the stream start at `PACE_FLOOR` and grow per round trip, so a jump from idle to 100 Mbit/s loses packets in its first 200 ms (×1.25: 1,377 of the first 1,786; ×2: 694). Starting unlimited fixes fast links but overruns the spec's 38 + 80 Mbit/s links. Performance mode loses nothing in the same jump in netsim. With the real daemons it can: a performance stream that starts as the lanes come up, before any lane's rate is known, sometimes runs late in its first second (about one harness run in three: frames up to 100 ms late, 3 to 6 of 1,200 unrecovered). The harness reports a stream's first 3 s without judging them.
2. **A12's CPU budget.** At 100 Mbit/s each way a daemon uses 33-42% of a core in a cloud container and 51-63% on CI's runners; at the 30 Mbit/s of a stream, 11-17%. Measured from `/proc/PID/stat`, 85% of it is kernel time and 6% of a core is the daemon's own code, so no rewrite of that code changes it. `strace -c` counts about seven system calls a round-trip packet (`ppoll`, a TUN `read`, a `sendmmsg` per lane, a `recvmmsg` per lane, a TUN `write`): every packet wakes the loop, because packets arrive one at a time. The harness also bills the daemon for the kernel's work on the packet along the whole simulated path (NAT, the bridges, the next namespace), work that other machines do in the field, so its figure is an upper bound.
   Decided for now: no coalescing. Waiting up to 0.25-0.5 ms before each turn would batch several packets a turn and cut the calls by about half, at up to 1-2 ms of added round trip, and at a stream's rate the daemons are inside the budget already. The harness keeps 25% as its default (`WIREBOND_A12_CPU`); CI's runners hold A12 to 85%, a guard against a regression rather than the budget. The budget itself is to be measured on a device in the field run. Above about 500 Mbit/s the calls would need batching in earnest (SPEC §26).
3. **A4 (bandwidth, bulk TCP).** Reorder wait p99 is 25-30 ms against 18 ms, and one seed reaches 96.4 of 99 Mbit/s.

4. **Releasing 2.0.0, and the full set's three failures.** A pushed tag publishes only after `ci.yml` with `long: true` (netsim's full set) and `system.yml` pass (2.0.0-rc.1 went out by hand, below). The full set fails in three tests:
   - `a5_bandwidth`, 15 of 20 seeds: the longest frame gap is 615-628 ms against 130 ms.
   - `a6_bandwidth`, 19 of 20 seeds: about 47 frames stay unrecovered from 1 s after the drops begin, against at most 1.
   - `swarm`, seed 8 in liveness mode (performance): 5 s after the faults heal, video has a 1,000.8 ms gap against 1,000 ms. The other 39 runs pass. CI never showed it, because the nightly stopped at `accept`; `fuzz`, `frag`, `jump`, `selftest` and `sleep_ring` pass at full length.

   Reproduce one: `WIREBOND_LONG=1 WIREBOND_SEED=1 cargo test -p wirebond-netsim --release --test accept -- --exact a5_bandwidth`. Either fix bandwidth's failover over every seed, or run the full set only for final releases (tags without `-`) and tag `v2.0.0-rc.1` now. Until a release exists, the one-line installers have nothing to download. Meanwhile a release candidate can go out by hand: a manual run of `release.yml` given a tag (Actions, release, Run workflow, `tag`) publishes after `ci.yml`'s checks and `system.yml`, without the full set, and says so in the release's notes. A pushed tag is gated as before.

5. **A lane that misses its punch round waits for the next one.** A candidate pair is probed only during a punch round: for 5 s after a link or candidate change, a wake, a new control channel, or the first datagram after 5 s idle. A path that opens later while data keeps flowing is not found until one of those happens, and a client that captures everything is seldom idle for 5 s. The harness met this by blocking the direct lanes as soon as the bond was up (on one CI run the cell's lanes stayed `punching` through the bonded run, which then matched the baseline); it now lets every lane validate first. A round every so often while a pair is unvalidated would close it.

Known limitations and gaps:

- **§14.1:** a copy queued in the sender's pacer on the lower-base-OWD lane can arrive after its gap was given up; it is then delivered out of order.
- **Moonlight's 1,392-byte video payloads** exceed the 1,372 bytes a 1,400-byte TUN carries whole, so each is sent as two fragments. They are delivered correctly; the cost is CPU.
- **The mesh lane:** a full lane packet (1,461 bytes) exceeds Tailscale's 1,280-byte MTU, so the OS fragments it.
- **Not done:** the six engine test vectors regenerated from Rust; the GPUI viewer (`crates/view`).
- **Review of the install work:** an adversarial review (installers, the Rust changes, CI) reported 18 defects, one of them twice. The commits after 60d0088 fix them, except three: a release published mid-install fails the checksum (both installers now say to run again), and Windows Server 2016's TLS and a failed PATH write's message, noted above. Its docs-accuracy pass did not finish: README's Install and Set up, SPEC §19 and §22 and the installers' printed next steps are worth one more read against the code.
- **Not yet run on real hardware:** macOS and Windows code has run only in CI (unit tests and the installers' service install); ICS on Windows with Wintun, pf on macOS and the field run (§23.1 tier 4) remain. The ARM builds (Linux aarch64, armv6/7; Windows ARM64) are built and linted but never run.
- **Install, untested paths:** Windows's "changing wirebond needs an administrator PowerShell" (CI's runner is always an administrator); `install.ps1` on Windows Server 2016, which needs TLS 1.2 turned on before `irm` can fetch it; Linux without systemd is refused by the installer, but a manual `wirebond install` there leaves its files until `wirebond uninstall`. On Windows, if writing the machine PATH fails, install still says a new PowerShell will find `wirebond` (uninstall does name a PATH entry it could not remove).

## The owner's steps

1. Settle the spec gaps above and in notes/2.0-build.md.
2. When the daemon brings bonds up, run `sudo tests/system/run.sh` locally and on CI; read the
   tables, and decide on A11's noise if it bites.
3. Run tier 4 by hand before 2.0.0 ships (SPEC §23.1): the Mac at a hotel, Wi-Fi plus an iPhone on
   USB, bonded to xanth; Moonlight at 10.77.0.1.
4. Decide Open 4, then tag `v2.0.0-rc.1` (and `v2.0.0` after the field run); release.yml publishes only once its checks pass, and the installers then work.
5. Delete the stale `spec/next` branch on GitHub (Branches page): this session's git access may not delete branches.
