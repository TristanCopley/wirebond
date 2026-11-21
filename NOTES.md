# wirebond 2.0: notes

Where the 2.0 build stands, the decisions taken while building it that SPEC.md does not record,
and what the owner has to do. SPEC.md is the specification; notes/2.0-build.md is the record of the
build's hand-off (the interfaces agreed then, who owned which file, and what is still open from
it). The 1.x notes (notes/rust.md, notes/open.md and the Go-era notes) are gone, and git does not
hold them either: this repository's history starts with the commit "wirebond 2.0".
CONTEXT.md and RESEARCH.md are 1.x-era background (SPEC §25); CONTEXT.md says which of it still
holds.

## Status

| Part | State |
| --- | --- |
| Engine (`crates/core`) | SPEC §10–§15: both modes, the contribution statistics (first arrivals, rescues, reorder waits), every fragment of a datagram in one flow, later copies not opened (receive cost halved). 162 unit tests and the integration suites pass; one pair test (§14.1's known limitation) and the six vector tests are ignored with reasons. |
| Simulator (`crates/netsim`) | Real inner IP packets, the anchor as destination and as exit, IPv4 fragmentation. The PR seed set passes 10 of 11 (A4 is ignored, see Open); swarm 3 of 3; fragmented 1,392-byte video loses nothing in performance mode. The full set (`WIREBOND_LONG=1`, 20 seeds each, about 30 minutes; `release.yml` requires it and CI had never run it) passes 8, ignores A4 and fails `a5_bandwidth` and `a6_bandwidth`; see Open 4. Performance mode passes both. |
| Daemon, OS layer, CLI (`crates/wirebond`) | Roles and the bond lifecycle, control protocol v2, the TUN pump, capture and egress on Linux, macOS and Windows, the DNS relay (UDP and TCP), STUN and bottleneck groups, fail open, captive portals, sleep and wake, `pick`, the support role, the update schedule, the local API and remote views, every CLI verb, `install` among them. Its tests pass on Linux (122), macOS and Windows (about 100 there: the Linux-only ones are compiled out elsewhere). |
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

   Reproduce one: `WIREBOND_LONG=1 WIREBOND_SEED=1 cargo test -p wirebond-netsim --release --test accept -- --exact a5_bandwidth`. Either fix bandwidth's failover over every seed, or run the full set only for final releases (tags without `-`) and tag `v2.0.0-rc.1` now. Until a release exists, the one-line installers have nothing to download. Meanwhile a release candidate can go out by hand: a manual run of `release.yml` given a tag (Actions, release, Run workflow, `tag`) publishes after `ci.yml`'s checks and `system.yml`, without the full set, and says so in the release's notes. A pushed tag is gated as before.

5. **Where the local API listens: SPEC §20.1 against §19 and the code.** §20.1 says one endpoint, `/var/run/wirebond.sock` (owner root, group `wirebond`, mode 0660, install adding the installing user to the group) and `\\.\pipe\wirebond` on Windows. §19 and the code use two sockets in the state directory: `ctl.sock`, which only the service's owner may connect to, for changes, and `status.sock`, which anyone may connect to, for reads (`crates/wirebond/src/host/mod.rs`: `CTL_SOCK`, `STATUS_SOCK`; AF_UNIX on Windows too). There is no `wirebond` group and install adds no user to one. Either §20.1 takes §19's two sockets, or the code takes §20.1's endpoint and group.
6. **A notice for Wintun.** `Cargo.toml` says MIT and LICENSE has the MIT text, but each Windows executable embeds Wintun 0.14.1's signed `wintun.dll`, which is WireGuard LLC's under its "Prebuilt Binaries License", not MIT. That license (read at Wintun's tag 0.14.1, `prebuilt-binaries-license.txt`) asks for no notice or attribution. Its section 3 forbids modifying or reverse engineering the DLL and removing its notices, and allows distributing it only "alongside other software that uses the Software only via the Permitted API", `wintun.h`'s, which is how wirebond uses it; it ends on any breach, and WireGuard LLC may change its terms. Whether embedding the unmodified DLL in the executable, which writes it out byte for byte at install, counts as distributing it alongside is the owner's reading to make. A short third-party notice (in README or a `THIRD-PARTY-NOTICES` file, or Wintun's own license text beside the release assets) would tell a recipient that MIT does not cover that DLL; the license does not require one. Separately, the Rust crates built into every executable are each available under MIT or Apache-2.0 (`cargo metadata`); both licenses ask that their notices accompany copies, and no release carries them yet.

Spec problems this build found, for the owner (SPEC.md is the owner's to change):

- **§16:** `key` carries `floor` (the counter floor of §10.4), which the table does not list; `bond open` carries `measure` for pick's temporary bond (§18); the `view` reply carries the status in a `status` field.
- **§19:** "`--json` gives the §20.2 schema" holds for `status` only: other verbs print their one reply line (`ok`, `changed`, `message`, `error`), and `version --json` prints `{"version", "commit"}`.
- **§20.2:** `Problem` has a `msg` field, a client's bond has `addr6` and `anchor_addr6`, a lane has `reorder_p50_ms` and `reorder_p99_ms`, and `contribution` has `links`, all beyond the example (each is left out of the JSON while empty, so the example still round-trips).
- **§22.2, "Requirements checked at install":** `wirebond install` checks nothing; `install.sh` requires systemd and warns when `ip` or `nft` is missing. Neither names `curl`, which §22.3's update needs, nor `nmcli`.
- **§22.4:** config.json also holds an `update` object (`url`, `starts`, `trial`, `bad`: where updates come from, and the trial of §22.3), which the example does not show.

Known limitations and gaps:

- **§14.1:** a copy queued in the sender's pacer on the lower-base-OWD lane can arrive after its gap was given up; it is then delivered out of order.
- **Moonlight's 1,392-byte video payloads** exceed the 1,372 bytes a 1,400-byte TUN carries whole, so each is sent as two fragments. They are delivered correctly; the cost is CPU.
- **The mesh lane:** a full lane packet (1,461 bytes) exceeds Tailscale's 1,280-byte MTU, so the OS fragments it.
- **A stop without teardown on macOS and Linux.** launchd and systemd stop the daemon with SIGTERM, whose default action ends the process: the teardown of §3.2 step 7 and §5.4 (capture removed first, then `bond close`) runs only on Windows, where the service manager's stop request reaches the loop. The kernel takes the TUN device and the routes through it; what else a role installed stays until the daemon next starts and cleans up (`cleanup_stale`). Checked on Linux with an anchor: after SIGTERM the nftables table `inet wirebond`, `ip_forward=1` and the saved values under `/run/wirebond` were still there. By the code, a client leaves its two routing rules, table 7849's throw routes and the lane links' `rp_filter`, and on macOS the DNS service key, pf's anchor and enable reference and the forwarding sysctls stay. `wirebond uninstall` stops the service the same way and does not clean up either, so §22.2's "removes ... routes, rules, NAT, DNS settings" holds only when the role was turned off first, which README now says to do; a support's sharing (NetworkManager's shared connection, Internet Sharing) outlasts even a restart.
- **`missing_tool` is reported late, and too widely.** §5.3 has the daemon check for `ip` at start; it reports `missing_tool` when a TUN, capture or egress call fails for want of the tool, so a device with no role shows nothing. It also gives that code to any such call that fails with the error kind NotFound, such as macOS's "no default route".
- **Status fields the daemon never fills.** A lane's `dev_ms`, `tx_mbps`, `rx_mbps`, `first_arrivals` and `bytes_today` (§20.2) are always 0: `node`'s `lanes` does not set them, though the engine's snapshot has each lane's first arrivals. `wirebond status` therefore shows 0% first arrivals and 0.0 ms jitter on every lane.
- **Not done:** the six engine test vectors regenerated from Rust; the GPUI viewer (`crates/view`), whose release assets §22.1 lists. The comments of `crates/core` and `crates/netsim` were not brought to 2.0: they hold 247 citations of sections SPEC 2.0 does not have (206 and 41; `§7.x`, `§8.x`, `§15.x` and `§17.x` are the 1.x specification's), and name the Go sources (`Go: core/lane.go`) and the deleted notes (`notes/core.md`, `rust.md`, `open.md`, `netsim.md`) throughout. Review ids in comments (`R1-13`, `R2-21`) are from the 1.x reviews, whose documents are gone too; they still tie a rule to its test.
- **Review of the install work:** an adversarial review (installers, the Rust changes, CI) reported 18 defects, one of them twice. The commits after 60d0088 fix them, except three: a release published mid-install fails the checksum (both installers now say to run again), and Windows Server 2016's TLS and a failed PATH write's message, noted below. Its docs-accuracy pass is done: README's Install and Set up, SPEC §19 and §22, the installers' printed next steps and the comments of `crates/wirebond`, the workflows and the scripts were read against the code. What it found is fixed where it was a document or a comment (README, CONTEXT.md, notes/2.0-build.md, tests/system, the 1.x section numbers in `host`, `update` and the workflows) and listed in these notes where it is the specification's or the code's.
- **Not yet run on real hardware:** macOS and Windows code has run only in CI (unit tests and the installers' service install); ICS on Windows with Wintun, pf on macOS and the field run (§23.1 tier 4) remain. The ARM builds (Linux aarch64, armv6/7; Windows ARM64) are built and linted but never run.
- **Install, untested paths:** Windows's "changing wirebond needs an administrator PowerShell" (CI's runner is always an administrator); `install.ps1` on Windows Server 2016, which needs TLS 1.2 turned on before `irm` can fetch it; Linux without systemd is refused by the installer, but a manual `wirebond install` there leaves its files until `wirebond uninstall`. On Windows, if writing the machine PATH fails, install still says a new PowerShell will find `wirebond` (uninstall does name a PATH entry it could not remove). On Linux without `curl`, `install.sh` downloads with `wget` and installs, but the daemon's updates need `curl` and fail there; the installer warns about a missing `ip` or `nft`, not about `curl`.

## The owner's steps

1. Settle the open decisions and the spec problems above, and what notes/2.0-build.md still lists.
2. Read the system harness's tables (CI's `system` job prints them; `sudo tests/system/run.sh` runs
   it locally), and decide on A11's noise if it bites.
3. Run tier 4 by hand before 2.0.0 ships (SPEC §23.1): the Mac at a hotel, Wi-Fi plus an iPhone on
   USB, bonded to xanth; Moonlight at 10.77.0.1.
4. Decide Open 4, then tag `v2.0.0-rc.1` (and `v2.0.0` after the field run); release.yml publishes only once its checks pass, and the installers then work.
5. Delete the stale `spec/next` branch on GitHub (Branches page): this session's git access may not delete branches.
