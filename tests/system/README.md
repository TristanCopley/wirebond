# tests/system: the 2.0 system harness

SPEC §23.1 tier 3: the daemon as released, as client and anchor, in network namespaces, with real
TUN devices, nftables NAT, tc's tbf and netem; driven by `crates/load`. Linux, as root:

```
cargo build --release -p wirebond -p load
sudo tests/system/run.sh [net|a7|a8|a9|a10|a12|shaped|netem|daemons|all|clean]...
```

| Target | What it checks |
| --- | --- |
| `net` | The namespaces and routes, and the load tool against plain echoes, with no daemon. |
| `a7` | The bond down for 10 s, timed from the bond being fully down (§17: every lane down, which takes `DOWN_AFTER` without echoes, and the channel down): fail open within `FAIL_OPEN_AFTER` + 1 s and not before `FAIL_OPEN_AFTER`, a new stream going direct meanwhile; capture back within 3 s of a lane validating, and a new stream through the anchor again. Each poll's lane states are in `a7/polls.txt`. |
| `a8` | Client to the internet server through the anchor's NAT, UDP and TCP, IPv4 and IPv6; a forged source never arrives. |
| `a9` | Client to a service on the anchor at 10.77.0.1: delivered locally, never NATed. |
| `a10` | `pick` between two anchors: the better one wins in each mode; `--dry-run` changes nothing. Without netem the worse anchor is only narrower, so only bandwidth mode is checked. |
| `a12` | At most 25% of one core per daemon at 100 Mbit/s in performance with two lanes. |
| `shaped`, `netem` | 30 Mbit/s video at 120 fps on the shaped hotel and cell, against the mesh-lane baseline (A11), then UDP and TCP with CPU and RSS. |
| `daemons`, `all` | Every daemon scenario; `all` adds `net`. `clean` removes what dead runs left. |

The bond's checks send UDP datagrams the bond carries whole: 1372 bytes over IPv4 and 1352 over
IPv6, its MTU (`TUN_MTU`, 1400) less the headers. A 1400-byte datagram leaves the client's kernel
as two IP fragments, two packets through the bond; the engine keys the fragments after the first
apart from the first (SPEC §13), and fragments reordered by more than the kernel's
`ipfrag_max_dist` (64) fail reassembly. a8 measures such a stream beside its check, informative.

Every measurement records the kernel's counters before and after it in `drops.txt`, which a
diagnosis prints: UDP receive-buffer and send errors, IP discards, failed reassemblies and
fragments made in each namespace, each interface's and qdisc's drops (the TUN device's with its
queue length), and softnet's backlog drops.

The namespaces, the addresses and the shaping are drawn at the top of `run.sh`. Each scenario runs
in its own subshell: one that stops is recorded and the next still runs. At the end, `load table`
prints every result and verdict; each failed scenario is diagnosed (daemon statuses and logs,
addresses, rules, routes, nftables, qdiscs, echo counters). Environment: `WIREBOND_BIN`, `LOAD_BIN`,
`WIREBOND_MODE`, `WIREBOND_SECONDS`, `WIREBOND_KEEP=1` (keep the run directory), `WIREBOND_TRACE=1`.

`load help` lists the load tool's commands; NOTES.md records how it measures.
