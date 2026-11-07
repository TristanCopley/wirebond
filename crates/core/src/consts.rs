//! Every constant of SPEC §21, and the wire format's sizes and flags (§10.3, §11.2). Durations are
//! nanoseconds, like [`crate::Mono`]; rates are bits per second. The daemon and netsim use the
//! ones that concern them; netsim's acceptance thresholds live in netsim. Tunables the spec does
//! not name live, private, in the module that uses them.
//!
//! One owner edits this file (notes/2.0-build.md): the core engine's.

use crate::Mono;

pub const MS: Mono = 1_000_000;
pub const SEC: Mono = 1_000 * MS;
pub const MIN: Mono = 60 * SEC;

// The wire (§10.3, §11.2). Integers are big-endian.
pub const TYPE_DATA: u8 = 0x01; // data: one inner IP packet
pub const TYPE_PROBE: u8 = 0x02; // probe
pub const SMALL_BIT: u8 = 0x80; // on TYPE_DATA: a small-flow packet, delivered without reordering (§15)
pub const HEADER_LEN: usize = 11; // type (1) + rid (2) + counter (8): the AEAD's additional data
pub const TAG_LEN: usize = 16; // AES-256-GCM tag
pub const DATA_LEN: usize = 6; // flow (2) + seq (4) at the front of a data payload
pub const OVERHEAD: usize = HEADER_LEN + DATA_LEN + TAG_LEN; // 33 bytes added to each inner packet
pub const PROBE_LEN: usize = 16; // lane 2, seq 4, sent_at 8, flags 1, n 1
pub const ECHO_LEN: usize = 53; // one echo
pub const MAX_ECHOES: usize = (MAX_PACKET - HEADER_LEN - TAG_LEN - PROBE_LEN) / ECHO_LEN;
pub const PROBE_COPY: u8 = 1; // flags bit 0: this lane is in my copy set
pub const PROBE_ANSWER: u8 = 2; // flags bit 1: answer at once (validation)
pub const PROBE_REWIND: u8 = 4; // flags bit 2: a rewind of mine is in flight (§15)
pub const EPOCH_LABEL: &[u8] = b"wirebond epoch"; // K_e = HMAC-SHA256(K_{e-1}, EPOCH_LABEL)

// Ports and sizes.
pub const PORT: u16 = 7849; // control (TCP, on mesh addresses) and lanes (UDP)
pub const TUN_MTU: usize = 1400; // a lane packet fits 1500 bytes over IPv6 (1400 + 33 + 48)
pub const MAX_PACKET: usize = 1500; // largest lane packet
pub const MAX_INNER: usize = MAX_PACKET - OVERHEAD; // largest inner packet a data packet carries
pub const MAX_MSG: usize = 64 << 10; // one control-channel JSON line
pub const CTL_QUEUE: usize = 64; // control messages queued per channel before it is closed
pub const SOCK_BUF: usize = 4 << 20; // per UDP socket, each way
pub const BOND_BYTES: usize = 16 << 20; // all queues and buffers of one bond (§10.5 rule 4)
pub const MAX_BONDS: usize = 8; // per anchor
pub const MAX_LINKS: usize = 8; // per node
pub const MAX_CANDS: usize = 4; // per link

// Flows (§13).
pub const MAX_FLOWS: usize = 4096; // live flows per bond; another evicts the flow idle longest
pub const REORDER_SLOTS: usize = 2048; // held packets per flow
pub const FLOW_IDLE: Mono = 60 * SEC; // a flow number is reused after this without traffic

// Probes and punching (§9, §11).
pub const PROBE_ACTIVE: Mono = 20 * MS; // every lane while data flowed within IDLE_AFTER
pub const PROBE_PUNCH: Mono = 200 * MS; // every candidate pair, for PUNCH_FOR
pub const PUNCH_FOR: Mono = 5 * SEC; // after a link or candidate change, a wake or a new channel
pub const PUNCH_GAP: Mono = 5 * SEC; // at most one punch round per peer per PUNCH_GAP
pub const PROBE_IDLE: Mono = 25 * SEC; // one probe on one direct lane while idle
pub const IDLE_AFTER: Mono = 5 * SEC; // no data for this long: idle
pub const IDLE_MISSES: u32 = 3; // idle probes in a row without an answer: down
pub const ECHO_WINDOW: Mono = 2 * SEC; // a probe echoes every lane received on within this
pub const RTT_RING: usize = 256; // probe send times kept per lane
pub const ECHO_HISTORY: usize = 8; // echoes kept per lane for loss and delivery deltas
pub const MAD_SAMPLES: usize = 128; // samples kept for a median absolute deviation

// Estimates (§11.3).
pub const EWMA_GAIN: f64 = 1.0 / 8.0; // RTT smoothing
pub const DEV_WINDOW: Mono = 2 * SEC; // median absolute deviation of OWD over this
pub const MIN_WINDOW: Mono = 10 * SEC; // base OWD and minimum RTT over this
pub const RATE_WINDOW: Mono = 100 * MS; // loss and delivery rate

// Liveness (§11.4).
pub const SUSPECT_MIN: Mono = 20 * MS; // suspect after RTT + max(SUSPECT_MIN, SUSPECT_DEV × jitter)
pub const SUSPECT_DEV: f64 = 4.0;
pub const LAG_FLOOR: Mono = 10 * MS; // lagging after max(LAG_FLOOR, LAG_JITTER × dev) + ΔOWD
pub const LAG_JITTER: f64 = 3.0;
pub const DOWN_AFTER: Mono = 2 * SEC; // no echo for this long while active: down
pub const UP_AFTER: Mono = SEC; // clean echoes for this long: up again
pub const BLIND_AFTER: Mono = 500 * MS; // no echo on any lane: the blind pair
pub const WAKE_GAP: Mono = SEC; // sleep-inclusive time beyond the monotonic clock that is a wake
pub const WAKE_JUMP: Mono = 3 * SEC; // or a monotonic jump between ticks that is a wake

// Rate control and pacing (§12).
pub const TARGET_FLOOR: Mono = 20 * MS; // target = max(TARGET_FLOOR, TARGET_DEV × dev, while app-limited)
pub const TARGET_DEV: f64 = 2.0;
pub const LOSS_OVERUSE: f64 = 0.02; // loss over RATE_WINDOW that is overuse ...
pub const LOSS_MIN: u64 = 5; // ... with at least this many packets lost
pub const BETA: f64 = 0.85; // a cut is at least to BETA × rate
pub const STARTUP_GAIN: f64 = 1.25; // per RTT while backlogged, until the first overuse
pub const INCREASE_STEP: f64 = 0.01; // of rate per RTT after startup
pub const PACE_FLOOR: f64 = 5e6; // the lowest rate and pace
pub const PACE_BURST: Mono = MS; // token-bucket depth at pace, plus one packet
pub const QUEUE_AGE: Mono = 100 * MS; // a packet older than this in a pacer queue is dropped

// Classes (§13).
pub const SMALL_RATE: f64 = 1e6; // a flow direction under this over SMALL_WINDOW is small
pub const SMALL_WINDOW: Mono = 200 * MS;
pub const SMALL_COPY_BUDGET: f64 = 2e6; // small flows copied per bond at most

// Modes (§14, §15).
pub const DUP_GRACE: Mono = 2 * MS; // performance: a gap a lane passed waits the extra base OWD + this
pub const RESCUE_GAP: Mono = 10 * MS; // a first copy this much earlier than every other is a rescue
pub const BANDWIDTH_SKEW_MAX: Mono = 50 * MS; // base OWD within this of the fastest eligible lane
pub const ECF_MARGIN: Mono = MS; // a lane other than the fastest must win by this
pub const BANDWIDTH_FLOOR_HOLD: Mono = 500 * MS; // in overuse this long at PACE_FLOOR: barred
pub const BAR: Mono = 60 * SEC; // first bar, doubling while the lane keeps failing
pub const BAR_MAX: Mono = 10 * MIN;
pub const REWIND_WINDOW: Mono = 300 * MS; // or the lane's suspect timeout + RTT if longer
pub const REWIND_SHARE: f64 = 0.5;
pub const REORDER_MAX: Mono = 50 * MS; // the reorder timer's cap
pub const REORDER_RESCUE: Mono = 150 * MS; // while a carrying lane lags or a rewind is in flight
pub const REORDER_LANES: Mono = 100 * MS; // lanes that carried a flow within this must pass a gap
pub const SKEW_MARGIN: f64 = 1.25;
pub const SKEW_DECAY: u32 = 256;
pub const REORDER_SHARE: f64 = 0.75;

// Keys, replay and validation (§10).
pub const REPLAY_WINDOW: u64 = 8192; // counters per window: one per key, one per lane and key
pub const EPOCH_BITS: u32 = 24; // a key ratchets every 2^24 packets
pub const VALIDATE_AMP: u64 = 3; // an unvalidated address gets at most 3 × the bytes received from it

// Links and STUN (§9).
pub const LINK_SCAN: Mono = 2 * SEC; // interface listing period
pub const STUN_EVERY: Mono = 5 * MIN; // STUN refresh while data flows
pub const STUN_WAIT: Mono = 3 * SEC; // a STUN reply is accepted from a server queried within this

// Control channel and failure handling (§16, §17).
pub const KEEP_IDLE: Mono = 5 * SEC; // TCP keepalive: a vanished peer is found in about 11 s
pub const KEEP_INTERVAL: Mono = 2 * SEC;
pub const KEEP_COUNT: u32 = 3;
pub const REDIAL_MIN: Mono = SEC; // backoff from REDIAL_MIN to REDIAL_MAX
pub const REDIAL_MAX: Mono = 30 * SEC;
pub const ALL_LANES_DOWN: Mono = 2 * SEC; // every lane down this long while the channel is up: redial
pub const FAIL_OPEN_AFTER: Mono = 5 * SEC; // every lane and the channel down this long: remove capture
pub const LANES_BLOCKED_AFTER: Mono = 3 * SEC; // no lane validated this long after connect
pub const MESH_ONLY_AFTER: Mono = 30 * SEC; // only the mesh lane this long: mesh_only

// DNS relay (§7).
pub const DNS_INFLIGHT: usize = 256; // queries in flight per bond
pub const DNS_TIMEOUT: Mono = 5 * SEC; // a query unanswered this long is dropped

// pick (§18).
pub const PICK_SECONDS: Mono = 10 * SEC; // per candidate, unless --seconds
pub const PICK_BULK_BYTES: u64 = 25_000_000; // bandwidth filler per candidate at most
pub const PICK_EGRESS_TRIES: u32 = 5; // TCP connects per reference
pub const PICK_REFERENCES: [&str; 3] = ["1.1.1.1:443", "8.8.8.8:443", "9.9.9.9:443"];

// Status (§20).
pub const STATUS_EVENTS: usize = 20; // events kept and shown by status
pub const LOG_LINES: usize = 500; // the daemon's in-memory log ring

// Update (§22.3).
pub const UPDATE_FIRST: Mono = 5 * MIN; // first check after start
pub const UPDATE_EVERY: Mono = 24 * 60 * MIN; // then daily, ± UPDATE_JITTER
pub const UPDATE_JITTER: Mono = 60 * MIN;
pub const UPDATE_IDLE: Mono = 10 * MIN; // no traffic this long: apply a staged update
pub const UPDATE_HEALTHY: Mono = 60 * SEC; // healthy this long after a start: the update is good
pub const UPDATE_MAX_STARTS: u32 = 3; // starts without reaching healthy before rollback
pub const SMOKE_TIMEOUT: Mono = 10 * SEC; // <exe>.new version --json must answer within this
