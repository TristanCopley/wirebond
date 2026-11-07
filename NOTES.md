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
| System harness (`tests/system/run.sh`) | With the real daemons: A7, A8, A9, A10 and the shaped hotel pass (0 of 1,200 frames lost; frame delay p99 14 ms against the mesh-lane baseline's 120 ms). A12 loses nothing at 100 Mbit/s but uses 28-31% of a core against its 25% budget. netem runs on CI only. |
| CI | Public runners since the repository went public. Every job passes: lint, the Linux tests and netsim's PR seeds, clippy on every release target, the tests on Windows and on macOS (one at a time, 79 s), the installers on all three, and `system.yml`'s namespace harness. Nightly runs (the full acceptance set) start now that `main` holds the code. |

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

1. **Bandwidth under a sudden full-rate load.** Lanes that share the stream start at `PACE_FLOOR` and grow per round trip, so a jump from idle to 100 Mbit/s loses packets in its first 200 ms (×1.25: 1,377 of the first 1,786; ×2: 694). Starting unlimited fixes fast links but overruns the spec's 38 + 80 Mbit/s links. Performance mode loses nothing in the same jump.
2. **A12's CPU budget.** About 30% of a core per daemon at 100 Mbit/s, most of it the loop sleeping and waking 18-36 thousand times a second. Meeting 25% needs a short coalescing wait (up to about 0.5 ms of added delay); otherwise the budget becomes about 30%.
3. **A4 (bandwidth, bulk TCP).** Reorder wait p99 is 25-30 ms against 18 ms, and one seed reaches 96.4 of 99 Mbit/s.

4. **Releasing 2.0.0-rc.1, and bandwidth's failover.** `release.yml` publishes only after `ci.yml` with `long: true` (netsim's full set) and `system.yml` pass, and the full set fails in bandwidth mode only:
   - `a5_bandwidth`, 15 of 20 seeds: the longest frame gap is 615-628 ms against 130 ms.
   - `a6_bandwidth`, 19 of 20 seeds: about 47 frames stay unrecovered from 1 s after the drops begin, against at most 1.

   Reproduce one: `WIREBOND_LONG=1 WIREBOND_SEED=1 cargo test -p wirebond-netsim --release --test accept -- --exact a5_bandwidth`. Either fix bandwidth's failover over every seed, or run the full set only for final releases (tags without `-`) and tag `v2.0.0-rc.1` now. Until a release exists, the one-line installers have nothing to download.

Known limitations and gaps:

- **§14.1:** a copy queued in the sender's pacer on the lower-base-OWD lane can arrive after its gap was given up; it is then delivered out of order.
- **Moonlight's 1,392-byte video payloads** exceed the 1,372 bytes a 1,400-byte TUN carries whole, so each is sent as two fragments. They are delivered correctly; the cost is CPU.
- **The mesh lane:** a full lane packet (1,461 bytes) exceeds Tailscale's 1,280-byte MTU, so the OS fragments it.
- **Not done:** the six engine test vectors regenerated from Rust; the GPUI viewer (`crates/view`).
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
