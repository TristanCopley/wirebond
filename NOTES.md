# wirebond 2.0: notes

Where the 2.0 build stands, the decisions taken while building it that SPEC.md does not record,
and what the owner has to do. SPEC.md is the specification; notes/2.0-build.md fixes the build's
interfaces and who owns which file. The 1.x notes (notes/rust.md, notes/open.md and the Go-era
notes) are gone; their history stays in git.

## Status

| Part | State |
| --- | --- |
| Engine (`crates/core`) | SPEC §10–§15: both modes, the contribution statistics (first arrivals, rescues, reorder waits), every fragment of a datagram in one flow, later copies not opened (receive cost halved). 162 unit tests and the integration suites pass; one pair test (§14.1's known limitation) and the six vector tests are ignored with reasons. |
| Simulator (`crates/netsim`) | Real inner IP packets, the anchor as destination and as exit, IPv4 fragmentation. The PR seed set passes 10 of 11 (A4 is ignored, see Open); swarm 3 of 3; fragmented 1,392-byte video loses nothing in performance mode. The full set (`WIREBOND_LONG=1`, 20 seeds each; `release.yml` requires it) has three known failures. `accept`, 24 to 45 minutes on CI by the runner, passes 8, ignores A4 and fails `a5_bandwidth` and `a6_bandwidth`; see Open 4. Performance mode passes both. The six suites after it take about an eighth as long again: `frag`, `fuzz`, `jump`, `selftest` and `sleep_ring` pass, two bandwidth tests among them ignored as in the PR set, and `swarm` passes 39 of its 40 runs: seed 8 fails in liveness mode (Open 5). |
| Daemon, OS layer, CLI (`crates/wirebond`) | Roles and the bond lifecycle, control protocol v2, the TUN pump, capture and egress on Linux, macOS and Windows, the DNS relay (UDP and TCP), STUN and bottleneck groups, fail open, captive portals, sleep and wake, `pick`, the support role, the update schedule, the local API and remote views, every CLI verb, `install` among them. 122 tests pass on Linux, macOS and Windows. |
| Install (`install.sh`, `install.ps1`) | One-liners that download this machine's release asset, check it against `SHA256SUMS` and run `wirebond install` as an administrator. `wirebond` lands on PATH everywhere: `/usr/local/bin` on Linux, a `/usr/local/bin` link to the root-owned executable on macOS, the machine PATH on Windows; `wirebond uninstall` removes them. CI installs, checks and uninstalls for real on Linux (systemd), macOS (launchd) and Windows PowerShell 5.1 (SCM) on every push. They work once a release exists (see Open). |
| Load tool (`crates/load`) | Echo, paced UDP, 120 fps video frames, TCP bulk, CPU and RSS, wait, checks, tables. Standard library only; replaces the Go tool. |
| System harness (`tests/system/run.sh`) | With the real daemons: A7, A8, A9, A10 and the shaped hotel pass (0 of 1,200 frames lost; frame delay p99 14 ms against the mesh-lane baseline's 120 ms). A12 loses nothing at 100 Mbit/s but fails on every CI run (Open 2): its count of the lanes up after the run finds four, not two, and each daemon uses 25-64% of a core by the runner, against its 25% budget (33-38% in a cloud container, where the lanes are two). netem runs on CI only, and its bonded video failed there in 7 of 22 runs (Open 6). The harness said so while `system.yml` passed: on the 18 runs at 50698d7 it ended `failed: a12` 12 times, `failed: a12 netem` 5 times and `failed: a12 a8 netem` once (a8 by the harness's own doing, see the gaps), with 60 of its 62 checks passing on the 12. |
| CI | Public runners since the repository went public. On a push every `ci.yml` job passes: lint, the Linux tests and netsim's PR seeds, clippy on every release target, the tests on Windows and on macOS (one at a time, 79 s) and the installers on all three. `system.yml` fails on A12 on every run, and on about one run in three on netem as well (Open 2 and 6). It did before, unseen: on each of the 18 runs at 50698d7 the harness reported failures and the job passed, because its step ended as the `tee` it piped the harness into. Every workflow's bash steps now run with `pipefail`, and a failed harness fails the job, which then uploads the run directory. The nightly `ci.yml` run has failed every night since its first, on 2026-11-07, at the full acceptance set (Open 4). It stopped there after `accept`, the first suite; with `--no-fail-fast` it runs all seven. |

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
2. **A12's CPU budget, and its lanes.** At 100 Mbit/s each daemon's share of a core follows the machine. Over 22 runs on CI's runners (the 18 at 50698d7 and the four of the change that unmasked them) the client uses 25-55% and the anchor 29-64%, in two clusters: 9 runs at 51-55% and 60-64%, the other 13 at 25-41% and 29-44%. A cloud container measures 33-38%. No run met 25%; the nearest had 25.3% and 29.1%. This entry had recorded about 30%, most of it the loop sleeping and waking 18-36 thousand times a second. Meeting 25% needs a short coalescing wait (up to about 0.5 ms of added delay); a budget of about 30% instead would have held on 3 of the 22 runs at most, so either budget needs the machine it is measured on. Whatever the CPU, A12 fails on every CI run on its second check: it wants exactly two lanes up after the run, and CI's runners have IPv6, where the client has four, the hotel's and the cell's over IPv4 and over IPv6. Without IPv6 it has two and the check passes. So on CI A12 runs with four lanes up, not SPEC's two; whether the check or the four lanes is wrong is open. `system.yml` fails until both are settled.
3. **A4 (bandwidth, bulk TCP).** Reorder wait p99 is 25-30 ms against 18 ms, and one seed reaches 96.4 of 99 Mbit/s.

4. **Releasing 2.0.0-rc.1, and bandwidth's failover.** `release.yml` publishes only after `ci.yml` with `long: true` (netsim's full set) and `system.yml` pass, and neither does. The full set has three known failures:
   - `a5_bandwidth`, 15 of 20 seeds: the longest frame gap is 138 to 656 ms (615 to 656 ms on 13 of the seeds) against 130 ms.
   - `a6_bandwidth`, 19 of 20 seeds: 2 to 49 frames stay unrecovered from 1 s after the drops begin (41 to 49 on 13 of the seeds), against at most 1.
   - `swarm`, 1 of 40 runs: seed 8's liveness run, in performance mode, so the set no longer fails in bandwidth mode only (Open 5).

   `system.yml` fails on A12 (Open 2) and at times on netem (Open 6), now that a failed harness fails its job.

   Reproduce one: `WIREBOND_LONG=1 WIREBOND_SEED=1 cargo test -p wirebond-netsim --release --test accept -- --exact a5_bandwidth`. Either fix bandwidth's failover over every seed, or run the full set only for final releases (tags without `-`) and tag `v2.0.0-rc.1` now. The second no longer publishes by itself: `system.yml` has to pass too. Until a release exists, the one-line installers have nothing to download.

5. **The long swarm's seed 8.** With `WIREBOND_LONG=1` (5 simulated minutes a run) seed 8 fails in liveness mode: from 5 s after the faults healed, at 200 s, video had a 1000.8 ms gap, against at most 1000 ms. The seed runs in performance mode with 13 fault classes on. Its 30-second run passes, and the PR set (seeds 1-3) does not hold it; the nightly did not reach `swarm` until it ran with `--no-fail-fast`. Why the gap is 0.8 ms over has not been looked into. Reproduce: `WIREBOND_LONG=1 WIREBOND_SEED=8 cargo test -p wirebond-netsim --release --test swarm -- --exact swarm`.

6. **The netem run on CI.** Its bonded video failed in 7 of the same 22 runs, in three ways:
   - In 4 runs it lost nearly every frame, 1,190 to 1,198 of 1,200 unrecovered, as the mesh-lane baseline before it does. The cell's two lanes were still `punching` when the scenario ended, over 30 s after the direct lanes were unblocked. "Every lane back" had passed all the same: it waits for three lanes up, and with IPv6 the mesh lane and the hotel's two make three.
   - In 2 runs it lost nothing but was slower than its baseline, at p50 by 1 to 2 ms and once at p99 and p99.9 too. That baseline had decoded 2 and 1 of its 1,200 frames, so its percentiles are those frames'. This is A11's noise.
   - In 1 run it left 2 frames unrecovered, against at most 1.

   Why the cell's lanes stay `punching` has not been looked into. Until this is settled, about one `system.yml` run in three reports `failed: a12 netem`.

Known limitations and gaps:

- **§14.1:** a copy queued in the sender's pacer on the lower-base-OWD lane can arrive after its gap was given up; it is then delivered out of order.
- **Moonlight's 1,392-byte video payloads** exceed the 1,372 bytes a 1,400-byte TUN carries whole, so each is sent as two fragments. They are delivered correctly; the cost is CPU.
- **The mesh lane:** a full lane packet (1,461 bytes) exceeds Tailscale's 1,280-byte MTU, so the OS fragments it.
- **The harness can stop a scenario itself:** on one of the 18 runs at 50698d7, a8's anchor daemon never started. Its log holds only `cp: cannot create regular file '…/stub-resolv.conf': File exists`, from `node_sh`, which copies `resolv.conf` into the run directory on every call; the daemon's start and the first status poll both make that call.
- **One Windows test is intermittent:** `node::dns::tests::tcp_queries_are_relayed_with_the_same_bounds` failed once in CI (2026-11-21, `rust windows-latest`), having passed the 18 runs at 50698d7 and the other run of the same commit. The relay it starts binds UDP port 0, then TCP on the port UDP got; when that TCP bind fails, the test indexes an empty list.
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
4. Decide Open 2, 4, 5 and 6, each of which fails a job release.yml waits for, then tag `v2.0.0-rc.1` (and `v2.0.0` after the field run); release.yml publishes only once its checks pass, and the installers then work.
5. Delete the stale `spec/next` branch on GitHub (Branches page): this session's git access may not delete branches.
