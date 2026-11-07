#!/usr/bin/env bash
# wirebond 2.0's system harness (SPEC §23.1 tier 3): the daemon as released, as client and anchor,
# in network namespaces with real TUN devices, nftables NAT, tc shaping and netem jitter, driven by
# crates/load. Linux, as root; tests/system/README.md has the details.
#
#   tests/system/run.sh [net|a7|a8|a9|a10|a12|shaped|netem|daemons|all|clean]...
#
#   net      the namespaces and the load tool against a plain echo, with no daemon
#   a7..a12  SPEC §23.3's harness scenarios; shaped, netem: the performance runs; daemons: all of these
#
# WIREBOND_BIN: the daemon (default target/release/wirebond); LOAD_BIN: the load tool (default
# target/release/load, built when missing); WIREBOND_MODE: performance (default) or bandwidth;
# WIREBOND_SECONDS: each measurement (10); WIREBOND_KEEP=1 keeps the run directory.
#
# Each run has its own namespaces (wbs-<pid>-i, -c, -a, -b, -s), removed on exit:
#
#   client (c)                         internet (i): a router            server (s)
#   hotel 192.168.1.2/24 ──── tbf ──── h0 192.168.1.1 ─┐
#   cell  172.20.10.2/28 ── netem ──── c0 172.20.10.1 ─┼── s0 198.51.100.1 ─── eth0 198.51.100.2/24
#                                                      ├── a0 203.0.113.1 ──── anchor (a) eth0 203.0.113.2/24
#                                                      └── b0 198.18.0.1 ───── anchor2 (b) eth0 198.18.0.2/24
#   tailscale0 100.64.0.1/24 ── mc ── mesh (bridge, MTU 1280) ── ma, mb ── tailscale0 100.64.0.2, .3
#
# IPv6 alongside: 2001:db8:1::/64 hotel, 2001:db8:2::/64 cell, 2001:db8:a::/64 and 2001:db8:b::/64
# the anchors' uplinks, 2001:db8:5::/64 the server, fd7a:115c:a1e0::/64 the mesh. The mesh rides
# the hotel, so its ports are shaped as the hotel is. The server runs `load echo` (UDP 47998,
# 47999 and 53, TCP 47989): the internet service and the resolver of the anchors' DNS relay and of
# every node's captive-portal check. Each anchor runs one too (UDP 48000, TCP 48010): a service on
# the anchor itself (A9), and the baseline's end.
set -euo pipefail
[ -z "${WIREBOND_TRACE:-}" ] || set -x # WIREBOND_TRACE=1: every command, for debugging the harness

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
[ "$(id -u)" = 0 ] || { echo "run.sh: run as root: it makes network namespaces" >&2; exit 2; }
[ $# -gt 0 ] || set -- all

prefix=wbs
I=$prefix-$$-i C=$prefix-$$-c A=$prefix-$$-a B=$prefix-$$-b S=$prefix-$$-s
run=$(mktemp -d "${TMPDIR:-/tmp}/wirebond-system.XXXXXX")
results=$run/results.jsonl
mode=${WIREBOND_MODE:-performance}
secs=${WIREBOND_SECONDS:-10}
bin=${WIREBOND_BIN:-$repo/target/release/wirebond}
load=${LOAD_BIN:-}
failed=$run/failed.txt
scen=none
pids=() reported=0
declare -A pid=()

SERVER4=198.51.100.2 SERVER6=2001:db8:5::2
ANCHOR4=203.0.113.2 ANCHOR6=2001:db8:a::2
MESH_C=100.64.0.1 MESH_A=100.64.0.2 MESH_B=100.64.0.3
BOND_A4=10.77.0.1 BOND_A6=fd77:b0d::1
HOTEL4=192.168.1.2 CELL4=172.20.10.2
# The largest UDP payloads the bond carries whole: its MTU is TUN_MTU, 1400 (SPEC §5.3), less the
# IPv4 or IPv6 and UDP headers. A larger datagram leaves the client's kernel as two IP fragments,
# so every one is two packets through the bond, and the engine keys the fragments after the first
# apart from the first (§13): reordered against each other by more than the kernel's
# ipfrag_max_dist (64) fragments, the datagram fails reassembly (Ip.ReasmFails in drops.txt). The
# bond's checks send these; a8 measures a fragmented stream beside them, informative.
FIT4=1372 FIT6=1352
# IPv6 everywhere when the kernel has it; without it, the IPv6 halves of the checks are skipped.
v6=0
[ -d /proc/sys/net/ipv6 ] && v6=1

# clean removes the namespaces of runs whose driver is gone, and what still runs in them.
clean() {
	local ns p
	for ns in $(ip netns list | awk '{print $1}' | grep -E "^$prefix-[0-9]+-[icabs]$" || true); do
		p=${ns#"$prefix"-}
		p=${p%-*}
		if [ "$p" != $$ ] && ! kill -0 "$p" 2>/dev/null; then
			{ ip netns pids "$ns" | xargs -r kill -9; } 2>/dev/null || true
			ip netns del "$ns" 2>/dev/null && echo "run.sh: removed $ns, left by run $p" >&2
		fi
	done
}

# teardown saves what a diagnosis reads, then kills everything this scenario started and deletes
# its namespaces.
teardown() {
	local ns
	[ "$scen" != none ] && [ -d "$run/$scen" ] && snapshot "$run/$scen"
	{
		for p in "${pids[@]}"; do kill -9 "$p"; done
		for ns in "$S" "$B" "$A" "$C" "$I"; do
			ip netns pids "$ns" | xargs -r kill -9
			ip netns del "$ns"
		done
		wait
	} 2>/dev/null || true
	pids=()
	pid=()
}

finish() {
	local status=$?
	teardown
	if [ "$reported" = 0 ]; then report || status=1; fi
	if [ "$status" = 0 ] && [ "${WIREBOND_KEEP:-0}" != 1 ]; then
		rm -rf "$run"
	else
		echo "run.sh: logs, daemon state and results are in $run" >&2
	fi
	exit "$status"
}
trap finish EXIT
trap 'exit 130' INT TERM

# build finds or builds the load tool.
build() {
	if [ -z "$load" ]; then
		load=$repo/target/release/load
		if [ ! -x "$load" ]; then
			echo "run.sh: building the load tool" >&2
			(cd "$repo" && cargo build --release -p load --locked)
		fi
	fi
	[ -x "$load" ] || { echo "run.sh: no load tool at $load" >&2; exit 2; }
}

# needs_daemon checks the daemon is there before a scenario that runs it.
needs_daemon() {
	[ -x "$bin" ] || { echo "run.sh: no daemon at $bin (cargo build --release -p wirebond, or set WIREBOND_BIN)" >&2; exit 2; }
}

# veth NS NAME PEER_NS PEER MTU: a link from NS's NAME to PEER_NS's PEER, both up.
veth() {
	ip link add "$2" netns "$1" mtu "$5" type veth peer name "$4" netns "$3" mtu "$5"
	ip -n "$1" link set dev "$2" up
	ip -n "$3" link set dev "$4" up
}

# addr NS DEV ADDR4/LEN ADDR6/LEN
addr() {
	ip -n "$1" addr add "$3" dev "$2"
	[ "$v6" = 0 ] || ip -n "$1" addr add "$4" dev "$2" nodad
}

# route6 NS ARGS...: an IPv6 route, when there is IPv6.
route6() {
	local ns=$1
	shift
	[ "$v6" = 0 ] || ip -n "$ns" -6 route add "$@"
}

ns_new() {
	ip netns add "$1"
	ip -n "$1" link set lo up
	[ "$v6" = 0 ] || ip netns exec "$1" sysctl -qw net.ipv6.conf.all.accept_dad=0 net.ipv6.conf.default.accept_dad=0
}

# uplink NS N ADDR4 ADDR6 ROUTER4 ROUTER6 ROUTER_DEV: a host's eth0 to the router, its default route.
uplink() {
	ns_new "$1"
	veth "$1" eth0 "$I" "$7" 1500
	addr "$1" eth0 "$3/24" "$4/64"
	addr "$I" "$7" "$5/24" "$6/64"
	ip -n "$1" route add default via "$5"
	route6 "$1" default via "$6"
}

# mesh_port NS PORT N: the host's tailscale0 on the mesh bridge, 100.64.0.N and fd7a:115c:a1e0::N.
mesh_port() {
	veth "$1" tailscale0 "$I" "$2" 1280
	ip -n "$I" link set dev "$2" master mesh
	addr "$1" tailscale0 "100.64.0.$3/24" "fd7a:115c:a1e0::$3/64"
}

# network makes the namespaces and links of the drawing above.
network() {
	mkdir -p "$run/$scen"
	ns_new "$I"
	ip netns exec "$I" sysctl -qw net.ipv4.ip_forward=1
	[ "$v6" = 0 ] || ip netns exec "$I" sysctl -qw net.ipv6.conf.all.forwarding=1
	ip -n "$I" link add mesh mtu 1280 type bridge
	ip -n "$I" link set dev mesh up
	ns_new "$C"
	veth "$C" hotel "$I" h0 1500
	addr "$C" hotel "$HOTEL4/24" 2001:db8:1::2/64
	addr "$I" h0 192.168.1.1/24 2001:db8:1::1/64
	veth "$C" cell "$I" c0 1500
	addr "$C" cell "$CELL4/28" 2001:db8:2::2/64
	addr "$I" c0 172.20.10.1/28 2001:db8:2::1/64
	ip -n "$C" route add default via 192.168.1.1 dev hotel metric 100
	ip -n "$C" route add default via 172.20.10.1 dev cell metric 200
	route6 "$C" default via 2001:db8:1::1 dev hotel metric 100
	route6 "$C" default via 2001:db8:2::1 dev cell metric 200
	mesh_port "$C" mc 1
	uplink "$A" 2 "$ANCHOR4" "$ANCHOR6" 203.0.113.1 2001:db8:a::1 a0
	mesh_port "$A" ma 2
	uplink "$B" 3 198.18.0.2 2001:db8:b::2 198.18.0.1 2001:db8:b::1 b0
	mesh_port "$B" mb 3
	uplink "$S" 5 "$SERVER4" "$SERVER6" 198.51.100.1 2001:db8:5::1 s0
	echo_on "$S" server -udp 47998,47999,53 -tcp 47989
	echo_on "$A" anchor -udp 48000 -tcp 48010
	echo_on "$B" anchor2 -udp 48000 -tcp 48010
}

# echo_on NS NAME -udp PORTS -tcp PORTS: a load echo in NS, its counters in NAME-echo.json.
echo_on() {
	local ns=$1 name=$2 stats=$run/$scen/$2-echo.json
	shift 2
	ip netns exec "$ns" "$load" echo --udp "$2" --tcp "$4" --stats "$stats" >>"$run/$scen/$name-echo.log" 2>&1 &
	pids+=("$!")
	for _ in $(seq 50); do
		[ -s "$stats" ] && return 0
		sleep 0.1
	done
	echo "run.sh: the $name echo did not start" >&2
	return 1
}

# The hotel (SPEC §23.3): 38 Mbit/s with a 64 KB burst and 180 ms of buffer (tbf), 30 ms one way
# with netem when the kernel has it (plus jitter in the netem run). The cell: 8 ms slower, 80 Mbit/s,
# 0.5% random loss (netem); tbf at 80 Mbit/s alone without netem. tbf keeps its queue in its child:
# netem replaces that child, so netem's own child is a byte FIFO of tbf's size.
hotel_qdisc() { # NS DEV DELAY
	local rate=38 burst=64 latency=180
	ip netns exec "$1" tc qdisc add dev "$2" root handle 1: tbf rate "${rate}mbit" burst "${burst}kb" latency "${latency}ms"
	if [ -n "$3" ]; then
		# shellcheck disable=SC2086 # DELAY is netem's delay arguments
		ip netns exec "$1" tc qdisc add dev "$2" parent 1:1 handle 10: netem delay $3
		ip netns exec "$1" tc qdisc add dev "$2" parent 10:1 handle 100: \
			bfifo limit $((rate * 1000000 / 8 * latency / 1000 + burst * 1024))
	fi
}

cell_qdisc() { # NS DEV NETEM
	if [ "$3" = 1 ]; then
		ip netns exec "$1" tc qdisc add dev "$2" root netem delay 38ms rate 80mbit loss 0.5%
	else
		ip netns exec "$1" tc qdisc add dev "$2" root tbf rate 80mbit burst 64kb latency 180ms
	fi
}

# has QDISC ARGS... is whether tc can add this qdisc here.
has() {
	local ns=$prefix-$$-probe ok=0
	ip netns add "$ns"
	ip -n "$ns" link add name probe0 type veth peer name probe1
	ip netns exec "$ns" tc qdisc add dev probe0 root "$@" 2>/dev/null || ok=1
	ip netns del "$ns"
	return $ok
}

# shape NETEM JITTER: the hotel (and the mesh riding it) and the cell, both ways.
shape() {
	local delay='' mesh=''
	if [ "$1" = 1 ]; then
		delay="30ms${2:+ $2}" mesh="31ms${2:+ $2}"
	fi
	hotel_qdisc "$C" hotel "$delay"
	hotel_qdisc "$I" h0 "$delay"
	hotel_qdisc "$C" tailscale0 "$mesh"
	hotel_qdisc "$I" mc "$mesh"
	cell_qdisc "$C" cell "$1"
	cell_qdisc "$I" c0 "$1"
}

# node_sh runs a command as an installed service runs (SPEC §22.2): /usr/local/bin/wirebond, its
# state in /var/lib/wirebond and its socket in /var/run, all bound over from this run's directory
# inside the private mount namespace ip netns exec makes, so nothing of this machine's is touched.
# /etc/resolv.conf, which the anchor's DNS relay reads, names the server's echo.
# shellcheck disable=SC2016 # expanded by the inner sh
node_sh='set -e
dir=$1
shift
target=$(readlink -f /etc/resolv.conf || echo /etc/resolv.conf)
case $target in /run/*)
	mkdir -p "$dir/run/$(dirname "${target#/run/}")"
	cp "$dir/resolv.conf" "$dir/run/${target#/run/}" ;;
esac
mount --bind "$dir/run" /var/run
case $target in /run/*) ;; *) mount --bind "$dir/resolv.conf" /etc/resolv.conf ;; esac
mount --bind "$dir/lib" /var/lib
mount --bind "$dir/bin" /usr/local/bin
exec "$@"'

ns_of() { case $1 in client) echo "$C" ;; anchor) echo "$A" ;; anchor2) echo "$B" ;; esac; }
role_of() { case $1 in client) echo client ;; *) echo anchor ;; esac; }

# node SIDE CMD...: CMD in SIDE's namespace, with SIDE's files.
node() {
	local side=$1
	shift
	ip netns exec "$(ns_of "$side")" env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin RUST_BACKTRACE=1 \
		sh -c "$node_sh" sh "$run/$scen/$side" "$@"
}

# cli SIDE ARGS...: the wirebond CLI against SIDE's daemon.
cli() {
	local side=$1
	shift
	node "$side" /usr/local/bin/wirebond "$@"
}

# config SIDE: config.json v2 (SPEC §22.4). Anchors allow the client's mesh address; the client
# bonds to the first anchor (A10 starts it on the second). No STUN server answers here; no
# release origin answers at update.url.
config() {
	local common='"stun": [], "mesh": ["100.64.0.0/10", "fd7a:115c:a1e0::/48"], "update": {"url": "http://127.0.0.1:9"}'
	case $1 in
	client) printf '{"v": 2, "role": "client", "anchor": "%s", "mode": "%s", "anchors": ["%s", "%s"], %s}\n' \
		"${2:-$MESH_A}" "$mode" "$MESH_A" "$MESH_B" "$common" ;;
	*) printf '{"v": 2, "role": "anchor", "allow": ["%s"], %s}\n' "$MESH_C" "$common" ;;
	esac
}

# daemon SIDE [ANCHOR]: `wirebond run` in SIDE's namespace. It is up once its status answers with
# the role its config gives.
daemon() {
	local side=$1 dir=$run/$scen/$1
	mkdir -p "$dir/lib/wirebond" "$dir/run" "$dir/bin"
	[ -e "$dir/bin/wirebond" ] || cp "$bin" "$dir/bin/wirebond"
	echo "nameserver $SERVER4" >"$dir/resolv.conf"
	config "$@" >"$dir/lib/wirebond/config.json"
	node "$side" /usr/local/bin/wirebond run >>"$dir/daemon.log" 2>&1 &
	pids+=("$!")
	if ! wait_for "$side" 10s "$side daemon answers" "role==$(role_of "$side")"; then
		echo "run.sh: the $side daemon ($bin) did not answer within 10 s; the end of its log:" >&2
		tail -n 20 "$dir/daemon.log" >&2
		return 1
	fi
	# $! is the subshell running node; the CPU and RSS samples need the daemon itself.
	local p
	for p in $(ip netns pids "$(ns_of "$side")"); do
		if [ "$(tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null)" = "/usr/local/bin/wirebond run " ]; then
			pid[$side]=$p
		fi
	done
	[ -n "${pid[$side]:-}" ] || { echo "run.sh: the $side daemon has no process in its namespace" >&2; return 1; }
}

# wait_for SIDE TIMEOUT LABEL COND...: until SIDE's `wirebond status --json` (the §20.2 status
# itself, SPEC §19) meets every condition; recorded in the results.
wait_for() {
	local side=$1 timeout=$2 label=$3
	shift 3
	local conds=()
	for c in "$@"; do conds+=(--for "$c"); done
	"$load" wait "${conds[@]}" --timeout "$timeout" --label "$scen: $label" --out "$results" -- \
		ip netns exec "$(ns_of "$side")" env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin \
		sh -c "$node_sh" sh "$run/$scen/$side" /usr/local/bin/wirebond status --json >/dev/null
}

# bonded: both daemons up and the client's bond up with a lane validated.
bonded() {
	daemon anchor
	daemon anchor2
	daemon client "$@"
	wait_for client 20s "the bond is up" 'bond.state==up' 'bond.lanes.*.state==up' ||
		{ echo "run.sh: $scen: the client's bond did not come up within 20 s" >&2; return 1; }
}

# counters: where a datagram can die, one "where.key value" line each, in every namespace: IP and
# UDP's counters (snmp, snmp6: receive-buffer and send errors among them), each interface's packets,
# drops, errors and queue length (the TUN device's too), each qdisc's drops and overlimits; and
# softnet's backlog drops, which are global.
counters() {
	local side ns d=0 sq=0 x y
	for side in client:$C anchor:$A anchor2:$B server:$S internet:$I; do
		ns=${side#*:}
		# shellcheck disable=SC2016 # expanded by the inner sh and awk
		ip netns exec "$ns" sh -c 'w=$1
			awk -v w="$w" '\''/^(Ip|Udp):/ { if (seen[$1]++) { for (i = 2; i <= NF; i++) print w "." substr($1, 1, length($1) - 1) "." n[$1, i], $i } else for (i = 2; i <= NF; i++) n[$1, i] = $i }'\'' /proc/net/snmp
			[ ! -e /proc/net/snmp6 ] || awk -v w="$w" '\''$1 ~ /^(Ip6|Udp6)/ { print w "." $1, $2 }'\'' /proc/net/snmp6
			for d in /sys/class/net/*; do
				n=${d##*/}
				for f in rx_packets tx_packets rx_dropped tx_dropped rx_errors tx_errors; do
					echo "$w.$n.$f $(cat "$d/statistics/$f")"
				done
				echo "$w.$n.qlen $(cat "$d/tx_queue_len")"
			done
			tc -s qdisc | awk -v w="$w" '\''/^qdisc/ { q = w ".tc." $5 "." $2 $3 } /Sent/ { gsub(/[,()]/, ""); print q ".dropped", $7; print q ".overlimits", $9 }'\''
		' sh "${side%%:*}" 2>/dev/null || true
	done
	while read -r _ x y _; do
		d=$((d + 0x$x)) sq=$((sq + 0x$y))
	done </proc/net/softnet_stat
	echo "softnet.dropped $d"
	echo "softnet.squeezed $sq"
}

# drops LABEL BEFORE AFTER: the counters that moved between two `counters`, into drops.txt: every
# drop, error and overlimit, failed reassemblies and fragments made, and each side's UDP datagrams
# in and out.
drops() {
	{
		echo "== $1"
		awk 'NR == FNR { a[$1] = $2; next }
			$1 ~ /(Errors|Discards|NoPorts|NoRoutes|Fails|Timeout|FragCreates|dropped|errors|overlimits|squeezed|InDatagrams|OutDatagrams)$/ && $2 != a[$1] {
				printf "%s %d\n", $1, $2 - a[$1] }
			$1 ~ /qlen$/ && $2 != a[$1] { print $1, $2 }' "$2" "$3"
	} >>"$run/$scen/drops.txt"
}

# measure NS KIND ARGS...: a load measurement in NS, into the results; false when a check fails.
# The kernel's counters before and after it go to drops.txt (`drops`).
measure() {
	local ns=$1 c=$run/$scen/counters ok=0
	shift
	counters >"$c.before"
	ip netns exec "$ns" "$load" "$@" --out "$results" >/dev/null || ok=$?
	counters >"$c.after"
	drops "$(tail -n 1 "$results" | sed 's/.*"label":"\([^"]*\)".*/\1/')" "$c.before" "$c.after"
	return "$ok"
}

# check SCENARIO COMMAND...: a measurement whose checks decide the scenario.
check() {
	local s=$1
	shift
	"$@" || fail "$s"
}

# echoed NAME --check COND...: a verdict on NAME's echo counters once written since the last datagram.
echoed() {
	local name=$1
	shift
	sleep 0.5
	"$load" check "$run/$scen/$name-echo.json" --label "$scen: at the $name" --out "$results" "$@" >/dev/null
}

# fail SCENARIO records a failed check and the directory the end of the run diagnoses.
fail() { echo "$1 $scen" >>"$failed"; }

start() {
	scen=$1
	echo "run.sh: $scen" >&2
	network
}

# blocked ON|OFF NS: drops wirebond's lanes and control channel (UDP and TCP 7849) at NS's every
# interface, as a bond that loses every path does.
blocked() {
	if [ "$1" = on ]; then
		ip netns exec "$2" nft -f - <<-EOF
			table inet wbsys_block {
				chain in { type filter hook input priority -10; meta l4proto { tcp, udp } th dport 7849 drop; meta l4proto { tcp, udp } th sport 7849 drop; }
				chain out { type filter hook output priority -10; meta l4proto { tcp, udp } th dport 7849 drop; meta l4proto { tcp, udp } th sport 7849 drop; }
			}
		EOF
	else
		ip netns exec "$2" nft delete table inet wbsys_block
	fi
}

# lanes_blocked ON|OFF NS IF: drops the lanes (UDP 7849) on NS's interface IF only. On the anchor's
# eth0 that leaves the bond on the mesh lane alone, the baseline of P1; on its tailscale0 it leaves
# the direct lanes alone, with the control channel (TCP) still on the mesh.
lanes_blocked() {
	local t=wbsys_lanes_$3
	if [ "$1" = on ]; then
		ip netns exec "$2" nft -f - <<-EOF
			table inet $t {
				chain in { type filter hook input priority -10; iifname "$3" udp dport 7849 drop; iifname "$3" udp sport 7849 drop; }
				chain out { type filter hook output priority -10; oifname "$3" udp dport 7849 drop; oifname "$3" udp sport 7849 drop; }
			}
		EOF
	else
		ip netns exec "$2" nft delete table inet "$t"
	fi
}

direct_blocked() { lanes_blocked "$1" "$2" eth0; }

# captured: whether the client's capture is installed (SPEC §5.3: the rule to table 7849).
captured() { ip -n "$C" rule show | grep -q 'lookup 7849'; }

# ms_since T0: milliseconds since T0, a time from date +%s%N.
ms_since() { echo $((($(date +%s%N) - $1) / 1000000)); }

# record SCENARIO LABEL OK [FIELDS]: a result line the harness itself measured.
record() {
	printf '{"kind":"harness","label":"%s: %s","ok":%s%s,"checks":["ok==1"],"pass":%s}\n' \
		"$1" "$2" "$3" "${4:+,$4}" "$3" >>"$results"
	[ "$3" = true ] || fail "$1"
}

# net: the namespaces and routes, and the load tool against plain echoes with no daemon: the
# client reaches the server over the hotel, IPv4 and IPv6; the anchor reaches it; the mesh joins
# client and anchor.
net() {
	start net
	check net measure "$C" udp --to "$SERVER4:47998" --mbps 10 --duration 2 --label "net: client to server, UDP" \
		--check 'loss_pct<=0'
	[ "$v6" = 0 ] || check net measure "$C" udp --to "[$SERVER6]:47998" --mbps 10 --duration 2 \
		--label "net: client to server, UDP over IPv6" --check 'loss_pct<=0'
	check net measure "$C" video --to "$SERVER4:47998" --mbps 30 --fps 120 --duration 2 \
		--label "net: client to server, 30 Mbit/s video" --check 'unrecovered==0'
	check net measure "$C" tcp --to "$SERVER4:47989" --bytes 20000000 --label "net: client to server, TCP"
	check net measure "$A" udp --to "$SERVER4:47998" --mbps 10 --duration 1 --label "net: anchor to server" --check 'loss_pct<=0'
	check net measure "$C" udp --to "$MESH_A:48000" --mbps 10 --duration 1 --label "net: client to anchor over the mesh" \
		--check 'loss_pct<=0'
	local six=()
	[ "$v6" = 0 ] || six=(--check "udp:47998:2001:db8:1::2.pkts>0")
	check net echoed server --check "udp:47998:$HOTEL4.pkts>0" --check "tcp:47989:$HOTEL4.conns==1" \
		--check "udp:47998:$ANCHOR4.pkts>0" "${six[@]}"
	if has netem delay 1ms; then
		shape 1 ''
		check net measure "$C" udp --to "$SERVER4:47998" --mbps 20 --duration 3 --label "net: the shaped hotel, 20 Mbit/s" \
			--check 'loss_pct<=1' --check 'p50_ms>=59' --check 'p50_ms<=75'
	else
		echo "run.sh: net: no netem here: the shaped hotel was not measured" >&2
	fi
	teardown
}

# client_status: the client's `status --json`, into the file st reads.
client_status() { cli client status --json >"$run/$scen/client.json" 2>/dev/null || true; }

# st COND...: whether the status client_status read last meets every condition.
st() {
	local c conds=()
	for c in "$@"; do conds+=(--check "$c"); done
	"$load" check "$run/$scen/client.json" "${conds[@]}" >/dev/null 2>&1
}

# chan_up: whether the client's control channel (TCP to port 7849, SPEC §16) is established.
chan_up() { [ -n "$(ip netns exec "$C" ss -Htn state established '( dport = :7849 )' 2>/dev/null)" ]; }

# ms_between T0 T1: milliseconds from T0 to T1, times from date +%s%N.
ms_between() { echo $((($2 - $1) / 1000000)); }

# lanes_until SCENARIO SECONDS LABEL OP N: until the number of the client's lanes that are up meets
# `test COUNT OP N`, recorded in the results with how long it took.
lanes_until() {
	local t0 n
	t0=$(date +%s%N)
	while :; do
		client_status
		n=$(sed 's/.*"lanes":\[//' "$run/$scen/client.json" | grep -o '"state":"up"' | wc -l)
		if [ "$n" "$4" "$5" ]; then
			record "$1" "$3" true "\"lanes_up\":$n,\"ms\":$(ms_since "$t0")"
			return 0
		fi
		if [ "$(ms_since "$t0")" -ge $(($2 * 1000)) ]; then
			record "$1" "$3 within $2 s" false "\"lanes_up\":$n"
			return 1
		fi
		sleep 0.1
	done
}

# a7: the bond down for 10 s (SPEC §23.3 A7, §17). Every path is blocked at once, so every lane is
# down DOWN_AFTER (2 s) after the block (§11.4: no echo for DOWN_AFTER), and the bond is fully down
# once the channel is too; fail open (bond_down, capture removed) comes within FAIL_OPEN_AFTER + 1 s
# of that, and not before FAIL_OPEN_AFTER. The lane states the status shows are recorded beside it
# (lanes_down_ms: none up or suspect) and in polls.txt. Then traffic goes direct: a new stream
# reaches the server from the hotel. The paths come back 10 s after the bond went down: capture
# within 3 s of a lane validating, and a new stream goes through the anchor again. A stream opened
# before the outage keeps the bond's source address, so it cannot go direct (§17: connections
# break when capture changes): its gap is recorded, not checked.
a7() {
	needs_daemon
	start a7
	bonded
	captured || record a7 "capture installed with the bond up" false
	ip netns exec "$C" "$load" udp --to "$SERVER4:47998" --mbps 2 --size 500 --duration 30 \
		--label "a7: a stream opened before the outage (informative)" --out "$results" >/dev/null &
	local stream=$! t0 now chan tl='' tc='' td='' to='' tv='' tk='' f ms
	pids+=("$stream")
	sleep 3
	blocked on "$A"
	blocked on "$B"
	t0=$(date +%s%N)
	# Polled about every 100 ms: the status, and the channel in the client's sockets; each poll's
	# bond and lane states go to polls.txt for the diagnosis.
	while [ "$(ms_since "$t0")" -lt 25000 ]; do
		now=$(date +%s%N)
		client_status
		chan=down
		! chan_up || chan=up
		echo "$(ms_between "$t0" "$now") ms: channel $chan, bond and lanes" \
			"$(grep -o '"state":"[a-z]*"' "$run/$scen/client.json" | cut -d'"' -f4 | tr '\n' ' ')" >>"$run/$scen/polls.txt"
		if [ -z "$tl" ] && st '!bond.lanes.*.state==up' '!bond.lanes.*.state==suspect'; then tl=$now; fi
		if [ -z "$tc" ] && [ "$chan" = down ]; then tc=$now; fi
		if st 'problems.*.code==bond_down' && ! captured; then
			to=$now
			break
		fi
		sleep 0.05
	done
	f="\"lanes_down_ms\":$(ms_between "$t0" "${tl:-$now}"),\"chan_down_ms\":$(ms_between "$t0" "${tc:-$now}")"
	if [ -n "$tc" ]; then
		td=$((t0 + 2000000000)) # DOWN_AFTER
		[ "$tc" -le "$td" ] || td=$tc
		f="$f,\"down_ms\":$(ms_between "$t0" "$td")"
	fi
	if [ -z "$to" ]; then
		record a7 "fail open within FAIL_OPEN_AFTER + 1 s of the bond down" false "$f"
		[ -n "$td" ] || td=$now
	elif [ -z "$td" ]; then
		record a7 "fail open only once the channel is down" false "$f,\"fail_open_ms\":$(ms_between "$t0" "$to")"
		td=$to
	else
		ms=$(ms_between "$td" "$to")
		f="$f,\"fail_open_ms\":$(ms_between "$t0" "$to"),\"ms\":$ms"
		record a7 "fail open within FAIL_OPEN_AFTER + 1 s of the bond down" "$([ "$ms" -le 6000 ] && echo true || echo false)" "$f"
		# Either end may be seen up to one poll late.
		record a7 "fail open not before FAIL_OPEN_AFTER" "$([ "$ms" -ge 4800 ] && echo true || echo false)" "$f"
		check a7 measure "$C" udp --to "$SERVER4:47999" --mbps 2 --size 500 --duration 1 \
			--label "a7: a new stream while the bond is down" --check 'loss_pct<=0'
		check a7 echoed server --check "udp:47999:$HOTEL4.pkts>0" --check "udp:47999:$ANCHOR4.pkts==0"
	fi
	ms=$((10000 - $(ms_since "$td")))
	[ "$ms" -le 0 ] || sleep "$((ms / 1000)).$(printf %03d $((ms % 1000)))"
	blocked off "$A"
	blocked off "$B"
	t0=$(date +%s%N)
	while [ "$(ms_since "$t0")" -lt 40000 ]; do
		now=$(date +%s%N)
		client_status
		if [ -z "$tv" ] && st 'bond.lanes.*.state==up'; then tv=$now; fi
		if [ -z "$tk" ] && captured; then tk=$now; fi
		if [ -n "$tv" ] && { [ -n "$tk" ] || [ "$(ms_between "$tv" "$now")" -gt 3000 ]; }; then break; fi
		sleep 0.05
	done
	if [ -z "$tv" ]; then
		record a7 "a lane validates within 40 s of the paths coming back" false
	elif [ -z "$tk" ]; then
		record a7 "capture back within 3 s of a lane validating" false "\"validated_ms\":$(ms_between "$t0" "$tv")"
	else
		ms=$(ms_between "$tv" "$tk")
		[ "$ms" -ge 0 ] || ms=0
		record a7 "capture back within 3 s of a lane validating" "$([ "$ms" -le 3000 ] && echo true || echo false)" \
			"\"validated_ms\":$(ms_between "$t0" "$tv"),\"ms\":$ms"
		sleep 1
		check a7 measure "$C" udp --to "$SERVER4:47999" --mbps 2 --size 500 --duration 1 \
			--label "a7: a new stream once capture is back" --check 'loss_pct<=0'
		check a7 echoed server --check "udp:47999:$ANCHOR4.pkts>0"
	fi
	wait "$stream" || true
	teardown
}

# a8: client to the internet server through the anchor's NAT: UDP and TCP, IPv4 and IPv6, all from
# the anchor's uplink address and nothing around the bond; a forged source never arrives.
a8() {
	needs_daemon
	start a8
	bonded
	check a8 measure "$C" udp --to "$SERVER4:47998" --mbps 5 --size "$FIT4" --duration 2 --label "a8: UDP over IPv4" \
		--check 'loss_pct<=0'
	measure "$C" udp --to "$SERVER4:47998" --mbps 5 --size 1400 --duration 2 \
		--label "a8: UDP over IPv4, 1400-byte datagrams the bond's MTU fragments (informative)" || true
	check a8 measure "$C" tcp --to "$SERVER4:47989" --bytes 5000000 --label "a8: TCP over IPv4"
	local six=()
	if [ "$v6" = 1 ]; then
		check a8 measure "$C" udp --to "[$SERVER6]:47998" --mbps 5 --size "$FIT6" --duration 2 --label "a8: UDP over IPv6" \
			--check 'loss_pct<=0'
		check a8 measure "$C" tcp --to "[$SERVER6]:47989" --bytes 5000000 --label "a8: TCP over IPv6"
		six=(--check "udp:47998:$ANCHOR6.pkts>0" --check "tcp:47989:$ANCHOR6.conns>0")
	else
		echo "run.sh: a8: no IPv6 in this kernel: IPv4 only" >&2
	fi
	check a8 measure "$C" udp --to "$SERVER4:47998" --from "$HOTEL4" --mbps 1 --duration 2 \
		--label "a8: a forged source (the hotel's address) into the bond" --check 'recv==0'
	check a8 echoed server --check "udp:47998:$ANCHOR4.pkts>0" --check "tcp:47989:$ANCHOR4.conns>0" "${six[@]}" \
		--check "udp:47998:$HOTEL4.pkts==0" --check "udp:47998:$CELL4.pkts==0" --check "tcp:47989:$HOTEL4.conns==0"
	teardown
}

# a9: client to a service on the anchor at its bond address: delivered locally, never NATed (the
# source is the client's bond address).
a9() {
	needs_daemon
	start a9
	bonded
	check a9 measure "$C" udp --to "$BOND_A4:48000" --mbps 5 --size "$FIT4" --duration 2 --label "a9: UDP to 10.77.0.1" \
		--check 'loss_pct<=0'
	check a9 measure "$C" tcp --to "$BOND_A4:48010" --bytes 5000000 --label "a9: TCP to 10.77.0.1"
	local six=()
	if [ "$v6" = 1 ]; then
		check a9 measure "$C" udp --to "[$BOND_A6]:48000" --mbps 5 --size "$FIT6" --duration 2 --label "a9: UDP to fd77:b0d::1" \
			--check 'loss_pct<=0'
		six=(--check 'udp:48000:fd77:b0d::*.pkts>0')
	fi
	check a9 echoed anchor --check 'udp:48000:10.77.0.*.pkts>0' --check 'tcp:48010:10.77.0.*.conns>0' \
		"${six[@]}" --check 'udp:48000:10.77.0.1.pkts==0' \
		--check "!udp:48000:$ANCHOR4.pkts>0" --check '!udp:48000:100.64.*.pkts>0'
	# The server's UDP 53 answers every node's captive-portal lookups (SPEC §17): only the load ports.
	check a9 echoed server --check '!udp:4799*.pkts>0' --check '!tcp:*.conns>0'
	teardown
}

# a10: pick between the anchor and anchor2, which is 25 ms further and 10 Mbit/s narrow: the
# anchor wins in each mode; --dry-run changes nothing; without it the client moves. Without netem
# anchor2 is only narrow (tbf), which decides bandwidth mode alone: performance is not checked.
a10() {
	needs_daemon
	start a10
	local m out modes=(performance bandwidth) apply=$mode qdisc=(netem delay 25ms rate 10mbit) dev
	if ! has netem delay 1ms; then
		echo "run.sh: a10: no netem here: anchor2 is only narrow, so only bandwidth mode is checked" >&2
		modes=(bandwidth) apply=bandwidth qdisc=(tbf rate 10mbit burst 16kb latency 50ms)
	fi
	for dev in "$B eth0" "$I b0" "$B tailscale0" "$I mb"; do
		ip netns exec "${dev% *}" tc qdisc add dev "${dev#* }" root "${qdisc[@]}"
	done
	bonded "$MESH_B"
	for m in "${modes[@]}"; do
		out=$run/$scen/pick-$m.json
		cli client pick "$MESH_A" "$MESH_B" --mode "$m" --seconds 4 --dry-run --json >"$out" 2>>"$run/$scen/pick.log" || true
		check a10 "$load" check "$out" --label "a10: pick --mode $m --dry-run" --out "$results" \
			--check "pick.winner==$MESH_A" --check 'pick.applied==false' >/dev/null
	done
	# load reads JSON lines, and the daemon saves config.json indented: one line of it.
	tr -d '\n' <"$run/$scen/client/lib/wirebond/config.json" >"$run/$scen/config.json"
	check a10 "$load" check "$run/$scen/config.json" --label "a10: --dry-run changed nothing" \
		--out "$results" --check "anchor==$MESH_B" >/dev/null
	cli client pick "$MESH_A" "$MESH_B" --mode "$apply" --seconds 4 --json >"$run/$scen/pick.json" 2>>"$run/$scen/pick.log" || true
	check a10 "$load" check "$run/$scen/pick.json" --label "a10: pick" --out "$results" \
		--check "pick.winner==$MESH_A" --check 'pick.applied==true' >/dev/null
	check a10 wait_for client 20s "the client moved to the winner" "bond.anchor==$MESH_A" 'bond.state==up'
	teardown
}

# a12: CPU at 100 Mbit/s in performance with two lanes: at most 25% of one core per daemon. The echo
# returns every datagram, so each daemon carries 100 Mbit/s each way. The mesh lane (the first in
# the status) is blocked, so the two lanes are the hotel's and the cell's (performance sends every
# packet on each lane); an idle bond keeps its spare lanes dormant, so they are counted right after
# the run, while still active.
a12() {
	needs_daemon
	start a12
	bonded
	lanes_blocked on "$A" tailscale0
	wait_for client 15s "the mesh lane not up" '!bond.lanes.0.state==up' || fail a12
	check a12 measure "$C" udp --to "$SERVER4:47998" --mbps 100 --size "$FIT4" --duration "$secs" \
		--pids "client=${pid[client]},anchor=${pid[anchor]}" --label "a12: 100 Mbit/s, performance, two lanes" \
		--check 'loss_pct<=1' --check 'procs.client.cpu<=25' --check 'procs.anchor.cpu<=25'
	lanes_until a12 1 "two lanes up after the run" -eq 2 || true
	teardown
}

# perf NAME NETEM JITTER: the performance runs on the shaped hotel and cell. 30 Mbit/s video at
# 120 fps to the anchor through the bond, first with the direct lanes blocked (the baseline: the
# mesh lane alone), then with every lane: never worse than the baseline at p50, p99 and p99.9 (A11),
# at most 1 unrecovered frame; then UDP and TCP to the internet server, with CPU and RSS.
perf() {
	local name=$1 netem=$2 jitter=${3:-}
	needs_daemon
	start "$name"
	shape "$netem" "$jitter"
	bonded
	local p="client=${pid[client]},anchor=${pid[anchor]}" base
	direct_blocked on "$A"
	lanes_until "$name" 15 "the baseline: the mesh lane alone" -eq 1 || true
	measure "$C" video --to "$BOND_A4:48000" --mbps 30 --fps 120 --size "$FIT4" --duration "$secs" --pids "$p" \
		--label "$name: baseline video (mesh lane)" || true
	base=$(tail -n 1 "$results")
	direct_blocked off "$A"
	# The mesh lane stayed up: wait for the hotel's and the cell's lanes too.
	lanes_until "$name" 30 "every lane back" -ge 3 || true
	sleep 2
	local f against=()
	for f in frame_p50_ms frame_p99_ms frame_p999_ms; do
		against+=(--check "$f<=$(jq -r ".$f // 0" <<<"$base")")
	done
	check "$name" measure "$C" video --to "$BOND_A4:48000" --mbps 30 --fps 120 --size "$FIT4" --duration "$secs" \
		--pids "$p" \
		--label "$name: bonded video, $mode" --check 'unrecovered<=1' "${against[@]}"
	measure "$C" udp --to "$SERVER4:47998" --mbps 30 --size "$FIT4" --duration "$secs" --pids "$p" \
		--label "$name: UDP 30 Mbit/s to the server" || true
	measure "$C" tcp --duration "$secs" --to "$SERVER4:47989" --pids "$p" --label "$name: TCP to the server" || true
	teardown
}

shaped() { perf shaped 0; }

netem() {
	if has netem delay 1ms; then
		perf netem 1 '5ms'
	else
		echo "run.sh: netem: skipped, tc has no netem here (on Ubuntu: linux-modules-extra-\$(uname -r))" >&2
	fi
}

daemons() {
	a7
	a8
	a9
	a10
	a12
	shaped
	netem
}

# snapshot DIR: what a diagnosis prints, saved before the namespaces go.
snapshot() {
	local d=$1 side
	for side in client anchor anchor2; do
		[ -d "$d/$side" ] || continue
		cli "$side" status --json >"$d/$side/status.json" 2>&1 || true
	done
	{
		for ns in "$C" "$A" "$B"; do
			echo "== $ns"
			ip -n "$ns" addr 2>&1 || continue
			ip -n "$ns" rule 2>&1
			ip -n "$ns" route show table all 2>&1 | grep -v '^local\|^broadcast\|^multicast' || true
			ip netns exec "$ns" nft list ruleset 2>&1
			ip netns exec "$ns" tc -s qdisc 2>&1
		done
		echo "== $I"
		ip netns exec "$I" tc -s qdisc 2>&1 || true
	} >"$d/net.txt" 2>&1
}

# diagnose SCENARIO prints, for CI's log, what its run directory holds: each daemon's last status
# and the end of its log, the addresses, rules, routes, nftables and qdiscs, and the echoes' counters.
diagnose() {
	local d=$run/$1 f
	echo "==== diagnosis: $1"
	for f in "$d"/*/status.json; do
		[ -e "$f" ] && { echo "--- $f"; cat "$f"; }
	done
	for f in "$d"/*/daemon.log; do
		[ -e "$f" ] && { echo "--- $f (last 40 lines)"; tail -n 40 "$f"; }
	done
	for f in "$d"/*-echo.json; do
		[ -e "$f" ] && { echo "--- $f"; cat "$f"; }
	done
	[ ! -e "$d/polls.txt" ] || { echo "--- $d/polls.txt (A7: from the block)"; cat "$d/polls.txt"; }
	[ ! -e "$d/drops.txt" ] || { echo "--- $d/drops.txt (the kernel's counters that moved in each measurement)"; cat "$d/drops.txt"; }
	[ -e "$d/net.txt" ] && { echo "--- $d/net.txt"; cat "$d/net.txt"; }
}

# report prints the results as tables and, when a check failed, each failed scenario's diagnosis.
report() {
	local s
	reported=1
	echo >&2
	[ -s "$results" ] && { "$load" table "$results" || true; }
	[ -s "$failed" ] || return 0
	for s in $(awk '!seen[$2]++ { print $2 }' "$failed"); do
		diagnose "$s"
	done
	echo "run.sh: failed: $(awk '{ print $1 }' "$failed" | sort -u | tr '\n' ' ')" >&2
	return 1
}

targets="net a7 a8 a9 a10 a12 shaped netem daemons all clean"
for target in "$@"; do
	case " $targets " in
	*" $target "*) ;;
	*)
		echo "run.sh: no target $target ($targets)" >&2
		exit 2
		;;
	esac
done
clean
[ "$*" = clean ] && exit 0
build
command -v jq >/dev/null || echo "run.sh: no jq: shaped and netem cannot compare with their baseline" >&2
# Each scenario runs in a subshell with errexit, so one that stops (a daemon that does not start, a
# bond that does not come up) is recorded and diagnosed, and the next one still runs.
expand() {
	local t
	for t in "$@"; do
		case $t in
		all) echo net a7 a8 a9 a10 a12 shaped netem ;;
		daemons) echo a7 a8 a9 a10 a12 shaped netem ;;
		clean) ;;
		*) echo "$t" ;;
		esac
	done
}
for target in $(expand "$@"); do
	scen=$target
	set +e
	(
		set -e
		trap - EXIT
		$target
	)
	status=$?
	set -e
	if [ "$status" != 0 ]; then
		echo "run.sh: $target stopped (exit $status)" >&2
		echo "$target $target" >>"$failed"
		teardown
	fi
done
scen=none
report || exit 1
