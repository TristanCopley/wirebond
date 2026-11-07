//! Traffic (Go netsim/apps.go): the datagram registry that tags every app datagram so the
//! invariants can tell each one apart and the metrics can time it; the Moonlight model (Sunshine's
//! frames with 20% Reed–Solomon parity as 0.8 Gbit/s bursts, Moonlight's purge and freeze, audio,
//! input on reliable ENet, the pings that open each flow); a saturating UDP sender; a TCP-like
//! paced sender (§17.2).
//!
//! The models live in the world ([`Apps`]) and are named by ids: `world[moon_id]`. Operations that
//! send or schedule are `World` methods taking the id; read-only metrics are methods on the model
//! (here and in metrics.rs). Owner: N3 (with metrics.rs and fake.rs).

use std::ops::{Index, IndexMut};

use crate::sim::{FlowId, INNER_HDR};
use wirebond_core::OVERHEAD;

use crate::link::IP_UDP;
use crate::node::DevId;
use crate::sim::{Dur, HookArgs, HookKind, MS, Rng, SEC, World, crc};

/// Every app datagram starts with a header that tags it: 0xA5, the stream, a 48-bit id, the
/// stream's sequence number, the send time and one word for the model (24 bytes, big-endian).
pub const HDR_LEN: usize = 24;
pub const TAG_MAGIC: u8 = 0xA5;

/// The id's bits in the tag's first word.
const ID_MASK: u64 = (1 << 48) - 1;

/// A delivered datagram's header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr {
    pub id: u64,
    pub seq: u32,
    pub sent: Dur,
    pub aux: u32,
}

/// A stream: index into the registry (Go *Stream; the id in the tag, so at most 256).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub u8);

/// What a stream does with each datagram core delivers on it: the device it was delivered at, the
/// flow, the header and the bytes. It gets the whole world (taken out of its stream while it runs).
pub type Recv = Box<dyn FnMut(&mut World, DevId, FlowId, Hdr, &[u8])>;

/// A stream's [`Recv`] from a closure; the bound gives the closure its signature.
fn on_recv(f: impl FnMut(&mut World, DevId, FlowId, Hdr, &[u8]) + 'static) -> Option<Recv> {
    Some(Box::new(f))
}

/// One direction of one app flow, as the registry knows it (Go Stream).
pub struct Stream {
    pub name: &'static str,
    pub port: u16,
    /// Delivered in order, so the order invariant applies.
    pub bulk: bool,
    pub sent: u64,
    pub received: u64,
    pub(crate) id: StreamId,
    pub(crate) recv: Option<Recv>,
    /// The order invariant's state: highest seq so far, the release's base and previous seq.
    pub(crate) max: u32,
    pub(crate) base: u32,
    pub(crate) prev: u32,
    /// The handle call of the current release.
    pub(crate) call: u64,
    /// Packets behind the newest in this call.
    pub(crate) late: u32,
}

/// What the registry remembers of one datagram (Go dgram).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Dgram {
    pub(crate) crc: u32,
    pub(crate) n: u16,
    /// The session pairing that sealed it; 0: never submitted.
    pub(crate) pair: u16,
    /// Submitted at, in ms.
    pub(crate) ms: u32,
    pub(crate) st: StreamId,
    pub(crate) done: bool,
    pub(crate) queued: bool,
}

/// Knows every app datagram the models made, to check what core delivers (Go registry).
#[derive(Default)]
pub struct Registry {
    pub(crate) streams: Vec<Stream>,
    pub(crate) dg: Vec<Dgram>,
}

/// The big-endian u32 at the start of b.
pub(crate) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// The big-endian u64 at the start of b.
pub(crate) fn be64(b: &[u8]) -> u64 {
    let mut x = [0; 8];
    x.copy_from_slice(&b[..8]);
    u64::from_be_bytes(x)
}

/// t in whole milliseconds, as the registry stores submit times.
fn reg_ms(t: Dur) -> u32 {
    (t / MS).clamp(0, Dur::from(u32::MAX)) as u32
}

/// d in seconds, as Go's Duration.Seconds computes it.
pub(crate) fn secs(d: Dur) -> f64 {
    (d / SEC) as f64 + (d % SEC) as f64 / 1e9
}

impl Registry {
    /// The datagram b's tag names, its stream and its header, if b carries a valid tag (Go find).
    pub fn find(&self, b: &[u8]) -> Option<(usize, StreamId, Hdr)> {
        if b.len() < HDR_LEN || b[0] != TAG_MAGIC {
            return None;
        }
        let t = be64(b);
        let (id, st) = (t & ID_MASK, ((t >> 48) & 0xff) as usize);
        if id >= self.dg.len() as u64 || st >= self.streams.len() {
            return None;
        }
        let h = Hdr { id, seq: be32(&b[8..]), sent: be64(&b[12..]) as Dur, aux: be32(&b[20..]) };
        Some((id as usize, StreamId(st as u8), h))
    }
}

impl World {
    /// Registers a stream; recv gets each datagram core delivers on it (Go Stream).
    pub fn stream(&mut self, name: &'static str, port: u16, bulk: bool, recv: Option<Recv>) -> StreamId {
        let n = self.reg.streams.len();
        let id = StreamId(u8::try_from(n).expect("at most 256 streams: the tag names one in a byte"));
        self.reg.streams.push(Stream {
            name,
            port,
            bulk,
            sent: 0,
            received: 0,
            id,
            recv,
            max: 0,
            base: 0,
            prev: 0,
            call: 0,
            late: 0,
        });
        id
    }

    /// A new tagged datagram of size bytes (at least HDR_LEN) on stream s (Go Datagram).
    pub fn datagram(&mut self, s: StreamId, seq: u32, aux: u32, size: usize, sent: Dur) -> Vec<u8> {
        let st = self[s].id;
        let mut b = vec![0; size.max(HDR_LEN)];
        let id = self.reg.dg.len() as u64;
        let tag = u64::from(TAG_MAGIC) << 56 | u64::from(st.0) << 48 | id;
        b[..8].copy_from_slice(&tag.to_be_bytes());
        b[8..12].copy_from_slice(&seq.to_be_bytes());
        b[12..20].copy_from_slice(&(sent as u64).to_be_bytes());
        b[20..24].copy_from_slice(&aux.to_be_bytes());
        self.reg.dg.push(Dgram { crc: crc(&b), n: b.len() as u16, st, ..Dgram::default() });
        b
    }

    /// The registry, for metrics that look datagrams up (inputCopies).
    pub fn registry(&self) -> &Registry {
        &self.reg
    }

    /// Records which session pairing sealed the datagram, the first time d submits it (Go
    /// registry.submit; pub for the self-tests).
    pub fn reg_submit(&mut self, d: DevId, b: &[u8]) {
        let Some((g, s, _)) = self.reg.find(b) else {
            return;
        };
        if self.reg.dg[g].queued {
            return;
        }
        let (pair, ms) = (self[d].pair_id, reg_ms(self.now));
        let g = &mut self.reg.dg[g];
        (g.pair, g.ms, g.queued) = (pair, ms, true);
        self[s].sent += 1;
    }

    /// Checks one datagram core delivered at d on f and passes it to its stream: nothing forged,
    /// corrupted or replayed, nothing twice, nothing across session pairings, the right port, bulk
    /// in order except one late packet after a skipped gap per handle call (§17.2); then hook
    /// Delivered and the stream's recv (Go registry.deliver; pub for the self-tests).
    pub fn reg_deliver(&mut self, d: DevId, f: FlowId, b: &[u8]) {
        let name = self[d].name;
        let Some((gi, s, h)) = self.reg.find(b) else {
            self.fail(format_args!("{name}: a forged datagram was delivered on flow {:08x} ({} bytes)", f.0, b.len()));
            return;
        };
        let (g, id) = (self.reg.dg[gi], h.id);
        let (sname, port, bulk) = (self[s].name, self[s].port, self[s].bulk);
        if usize::from(g.n) != b.len() || g.crc != crc(b) {
            self.fail(format_args!("{name}: a corrupted datagram was delivered on {sname}"));
        } else if !g.queued {
            self.fail(format_args!("{name}: datagram {id} on {sname} was delivered but never sent"));
        } else if g.done {
            self.fail(format_args!("{name}: datagram {id} on {sname} was delivered twice"));
        } else if self.pairs[usize::from(g.pair)] != [self[d].peer_inst, self[d].inst] {
            self.fail(format_args!(
                "{name}: datagram {id} on {sname}, sealed by an earlier session, was delivered (a replay)"
            ));
        } else if f.port() != port {
            self.fail(format_args!("{name}: datagram for port {port} was delivered to port {}", f.port()));
        }
        if self.fail.is_some() {
            return;
        }
        self.reg.dg[gi].done = true;
        self[s].received += 1;
        self.fire(&HookArgs { kind: HookKind::Delivered, dev: d, link: None, data: b, event: None });
        if bulk {
            self.reg_order(d, s, h.seq);
        }
        if let Some(mut recv) = self[s].recv.take() {
            recv(self, d, f, h, b);
            self[s].recv = Some(recv);
        }
    }

    /// The order invariant of a bulk stream: within one handle call a reorder buffer releases in
    /// order; a late packet after a skipped gap comes alone, on arrival.
    fn reg_order(&mut self, d: DevId, s: StreamId, seq: u32) {
        let (name, calls) = (self[d].name, self.calls);
        let st = &mut self[s];
        if st.call != calls {
            (st.call, st.base, st.prev, st.late) = (calls, st.max, st.max, 0);
        }
        let (sname, prev, behind) = (st.name, st.prev, seq <= st.base);
        if behind {
            st.late += 1;
        } else if seq > prev {
            st.prev = seq;
        }
        st.max = st.max.max(seq);
        if behind && st.late > 1 {
            self.fail(format_args!("{name}: {sname} delivered two packets behind its newest in one Handle call"));
        } else if !behind && seq <= prev {
            self.fail(format_args!("{name}: {sname} delivered seq {seq} after {prev} in one release"));
        }
    }

    /// Counts datagrams of s submitted in [from, to) that were never delivered (Go Lost).
    pub fn lost(&self, s: StreamId, from: Dur, to: Dur) -> usize {
        let (a, b) = (reg_ms(from), reg_ms(to));
        let lost = |g: &&Dgram| g.st == s && g.queued && !g.done && g.ms >= a && g.ms < b;
        self.reg.dg.iter().filter(lost).count()
    }
}

impl Index<StreamId> for World {
    type Output = Stream;
    fn index(&self, s: StreamId) -> &Stream {
        &self.reg.streams[s.0 as usize]
    }
}

impl IndexMut<StreamId> for World {
    fn index_mut(&mut self, s: StreamId) -> &mut Stream {
        &mut self.reg.streams[s.0 as usize]
    }
}

// ---------------------------------------------------------------------------------------------
// Moonlight

/// Moonlight ports and sizes (CONTEXT.md).
pub const PORT_VIDEO: u16 = 47998;
pub const PORT_CONTROL: u16 = 47999;
pub const PORT_AUDIO: u16 = 48000;
/// A 1,392-byte payload + Sunshine's 16-byte header.
pub const VIDEO_SIZE: usize = 1408;
pub const AUDIO_SIZE: usize = 336;
pub const INPUT_SIZE: usize = 48;
pub const PING_SIZE: usize = 32;
/// Sunshine's 0.8 Gbit/s burst: one video packet's time on the wire.
pub const SHARD_GAP: Dur = VIDEO_SIZE as Dur * 8 * SEC / 800_000_000;
pub const PING_EVERY: Dur = 500 * MS;
/// Sunshine stops a stream that has not pinged for this long.
pub const PING_TIMEOUT: Dur = 10 * SEC;

/// One video frame as Moonlight saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// Sunshine sent its first packet.
    pub sent: Dur,
    /// It became decodable; 0: never.
    pub done: Dur,
    /// Purged incomplete.
    pub lost: bool,
    /// Its frame time on each client link alone (§17.3).
    pub ideal: Vec<Dur>,
    pub(crate) got: u16,
}

/// One copied-flow datagram: when it was sent and delivered, and each client link's ground-truth
/// OWD in its direction when it was sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub sent: Dur,
    /// 0: never.
    pub got: Dur,
    pub owd: Vec<Dur>,
}

/// A Moonlight stream: index into the world's (Go *Moonlight).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MoonId(pub usize);

/// A Moonlight stream (§17.2): Sunshine's frames with 20% Reed–Solomon parity as 0.8 Gbit/s
/// bursts; Moonlight purging a frame that is incomplete when a packet of a later frame arrives,
/// and freezing one RTT per lost frame; audio every 5 ms; input on reliable ENet, resent after
/// RTT + 4 × variance with doubling; and the pings that open each flow.
#[derive(Clone, Debug, Default)]
pub struct Moonlight {
    pub bps: f64,
    pub fps: f64,
    /// Shards per frame: data, and total with parity.
    pub data: usize,
    pub total: usize,
    pub video: StreamId,
    pub audio: StreamId,
    pub input: StreamId,
    pub acks: StreamId,
    /// The pings on the video, audio and control flows.
    pub pings: Vec<StreamId>,
    pub frames: Vec<Frame>,
    /// Frames displayed.
    pub shown: Vec<Dur>,
    /// Frames purged.
    pub lost_at: Vec<Dur>,
    /// By ENet seq; `got` is when it was applied in order.
    pub inputs: Vec<Sample>,
    pub audios: Vec<Sample>,
    /// (end, length) of gaps over 20 ms between video datagrams.
    pub video_gaps: Vec<[Dur; 2]>,
    pub(crate) vflow: FlowId,
    pub(crate) aflow: FlowId,
    pub(crate) cflow: FlowId,
    /// The video flow's last ping.
    pub(crate) vping: Dur,
    pub(crate) vseq: u32,
    /// The frame being assembled.
    pub(crate) cur: u32,
    pub(crate) freeze: Dur,
    /// The last video datagram; −1: none.
    pub(crate) last_v: Dur,
    pub(crate) enet: Vec<EnetState>,
    pub(crate) rtt: Dur,
    pub(crate) rtt_var: Dur,
    pub(crate) applied: usize,
    pub(crate) moving: bool,
}

/// One ENet packet's reliability state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnetState {
    pub(crate) rto: Dur,
    pub(crate) tries: u32,
    pub(crate) acked: bool,
    pub(crate) arrived: bool,
}

/// A saturating sender: index into the world's (Go *Flood).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FloodId(pub usize);

/// A saturating UDP sender from the server toward the client at bps, on a flow the client opens
/// by pinging the port (Go Flood).
#[derive(Clone, Debug, Default)]
pub struct Flood {
    pub bps: f64,
    pub s: StreamId,
    /// Bytes delivered per 100 ms.
    pub got: Vec<i64>,
    pub(crate) flow: FlowId,
}

/// Flood's goodput buckets.
const FLOOD_BUCKET: Dur = 100 * MS;

impl Flood {
    /// The delivered rate in bits per second over [from, to) (Go Goodput).
    pub fn goodput(&self, from: Dur, to: Dur) -> f64 {
        let a = (from / FLOOD_BUCKET).max(0) as usize;
        let b = ((to / FLOOD_BUCKET).max(0) as usize).min(self.got.len());
        let bytes: i64 = self.got.get(a..b).map_or(0, |g| g.iter().sum());
        bytes as f64 * 8.0 / secs(to - from)
    }
}

/// A TCP-like sender: index into the world's (Go *Bulk).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BulkId(pub usize);

/// A bulk TCP sender from the server (§23.3): window-based and ack-clocked. It keeps at most
/// `cwnd` packets in flight past the newest seq the receiver acked, and sends the next ones as
/// acks open the window: slow start (one more packet per packet acked) up to `ssthresh`, then
/// CUBIC's growth (RFC 9438, Linux's default). A new hole in the receiver's count (a packet lost,
/// or given up behind a gap) cuts the window to BULK_BETA of itself, at most once per window of
/// data (the recovery point); an ack silence of BULK_RTO forgets what is in flight and starts
/// again from BULK_IW. There are no retransmissions: goodput counts what arrived.
#[derive(Clone, Debug, Default)]
pub struct Bulk {
    /// The congestion window and slow-start threshold, in packets.
    pub cwnd: f64,
    pub ssthresh: f64,
    pub s: StreamId,
    pub acks: StreamId,
    pub(crate) flow: FlowId,
    pub(crate) seq: u32,
    pub(crate) got: u32,
    /// The newest seq the receiver acked, and its count of packets received then.
    pub(crate) ack_seq: u32,
    pub(crate) ack_n: u32,
    /// The holes (acked seq − count) already answered by a cut, and the seq that ends recovery.
    pub(crate) holes: u32,
    pub(crate) recover: u32,
    pub(crate) last_ack: Dur,
    /// CUBIC's window before the last cut, and when that cut came.
    pub(crate) w_max: f64,
    pub(crate) epoch: Dur,
}

/// Bulk's initial window, in packets (RFC 6928).
const BULK_IW: f64 = 10.0;
/// Bulk's multiplicative decrease on loss, and its window growth's scale, packets per s³ (CUBIC's
/// beta and C, RFC 9438).
const BULK_BETA: f64 = 0.7;
const BULK_C: f64 = 0.4;
/// Bulk's largest window, in packets.
const BULK_MAX_CWND: f64 = 20_000.0;
/// No ack for this long: what is in flight is lost (a retransmission timeout).
const BULK_RTO: Dur = SEC;

/// Bulk's datagram size: a TCP segment's payload.
pub const BULK_SIZE: usize = 1392;

/// An echo load: index into the world's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EchoId(pub usize);

/// The system harness's UDP load (tests/system/load: `udp` against the server's `echo`): the
/// client sends `size`-byte datagrams at `bps`, evenly spaced, from one source port to the
/// server's port from start until stop, and the server returns each one it receives at once, on
/// the same flow. Each direction is one bulk flow, and what the server sends is what reached it.
#[derive(Clone, Debug, Default)]
pub struct Echo {
    pub bps: f64,
    pub size: usize,
    /// The client's datagrams, and the server's echoes of them.
    pub out: StreamId,
    pub back: StreamId,
    /// By the client's seq − 1: when it sent the datagram, and when its echo reached it (0: not
    /// yet).
    pub sent: Vec<Dur>,
    pub got: Vec<Dur>,
    /// The server's echoes so far: their seq, in its sending order.
    pub(crate) echoed: u32,
}

impl Echo {
    /// The datagrams the client sent in [from, to), and how many of their echoes reached it by
    /// `by`: the harness counts an echo until a second after its last send.
    pub fn counts(&self, from: Dur, to: Dur, by: Dur) -> (usize, usize) {
        let mut n = (0, 0);
        for (&s, &g) in self.sent.iter().zip(&self.got) {
            if s >= from && s < to {
                n.0 += 1;
                n.1 += usize::from(g > 0 && g <= by);
            }
        }
        n
    }

    /// The fraction of those datagrams whose echo had not reached the client by `by`.
    pub fn loss(&self, from: Dur, to: Dur, by: Dur) -> f64 {
        let (sent, back) = self.counts(from, to, by);
        if sent == 0 { 0.0 } else { (sent - back) as f64 / sent as f64 }
    }
}

/// The traffic models' state, owned by the world.
#[derive(Default)]
pub struct Apps {
    pub(crate) moon: Vec<Moonlight>,
    pub(crate) floods: Vec<Flood>,
    pub(crate) bulks: Vec<Bulk>,
    pub(crate) echoes: Vec<Echo>,
}

/// No flow yet: the client has not pinged it open.
const NO_FLOW: FlowId = FlowId(0);

impl World {
    /// Starts a stream of bps bits per second of video on the wire (Sunshine's parity included,
    /// notes/netsim.md 2) at fps frames per second, from start until stop (Go Moonlight).
    pub fn moonlight(&mut self, bps: f64, fps: f64, start: Dur, stop: Dur) -> MoonId {
        let m = MoonId(self.apps.moon.len());
        let total = (bps / 8.0 / fps / (VIDEO_SIZE - 16) as f64).ceil() as usize;
        let mut data = total;
        while data + (0.2 * data as f64).ceil() as usize > total {
            data -= 1;
        }
        self.apps.moon.push(Moonlight { bps, fps, data, total, last_v: -1, ..Moonlight::default() });
        let video = on_recv(move |w, _, _, h, _| w.moon_video(m, h));
        let video = self.stream("video", PORT_VIDEO, true, video);
        let audio = on_recv(move |w, _, _, h, _| w[m].audios[h.seq as usize].got = w.now);
        let audio = self.stream("audio", PORT_AUDIO, false, audio);
        let input = on_recv(move |w, _, f, h, _| w.moon_input(m, f, h));
        let input = self.stream("input", PORT_CONTROL, false, input);
        let acks = on_recv(move |w, _, _, h, _| w.moon_ack(m, h));
        let acks = self.stream("enet-ack", PORT_CONTROL, false, acks);
        let mo = &mut self[m];
        (mo.video, mo.audio, mo.input, mo.acks) = (video, audio, input, acks);
        let r = self.sub();
        // Moonlight pings each of its three flows (ENet's own ping on the control flow).
        for (i, port) in [PORT_VIDEO, PORT_AUDIO, PORT_CONTROL].into_iter().enumerate() {
            let ping = on_recv(move |w, _, f, _, _| w.moon_ping(m, f));
            let s = self.stream("ping", port, false, ping);
            self[m].pings.push(s);
            let src = 50000 + i as u16;
            self.every(start + i as Dur * MS, PING_EVERY, move |w| {
                if w[DevId::Client].awake() {
                    let b = w.datagram(s, 0, 0, PING_SIZE, w.now);
                    w.send_from(DevId::Client, port, src, b);
                }
                w.now < stop
            });
        }
        self.every(start, (1e9 / fps) as Dur, move |w| {
            w.moon_frame(m);
            w.now < stop
        });
        self.every(start, 5 * MS, move |w| {
            w.moon_audio(m);
            w.now < stop
        });
        // Input: the mouse moves for Exp(1 s), then rests for Exp(1 s); while it moves, one
        // batched packet every millisecond.
        self.at(start, move |w| moon_phase(w, m, r));
        self.every(start, MS, move |w| {
            let mo = &w[m];
            if mo.moving && w[DevId::Client].awake() && mo.cflow != NO_FLOW {
                let i = mo.enet.len();
                w.moon_send(m, i, true);
            }
            w.now < stop
        });
        m
    }

    /// Each client link's ground-truth OWD toward the server (up) or the client (Go m.owds).
    pub fn owds(&mut self, up: bool) -> Vec<Dur> {
        let n = self[DevId::Client].links.len();
        let mut o = Vec::with_capacity(n);
        for i in 0..n {
            let l = self[DevId::Client].links[i];
            o.push(self.owd(if up { l.up() } else { l.down() }));
        }
        o
    }

    /// A ping arrived at the server on f: it opens the video, audio or control flow (Go ping).
    pub(crate) fn moon_ping(&mut self, m: MoonId, f: FlowId) {
        let now = self.now;
        let mo = &mut self[m];
        match f.port() {
            PORT_VIDEO => (mo.vflow, mo.vping) = (f, now),
            PORT_AUDIO => mo.aflow = f,
            // The client sends input once the ENet flow is open.
            PORT_CONTROL => mo.cflow = f,
            _ => {}
        }
    }

    /// Sends one frame as Sunshine does, while the client keeps pinging (Go frame).
    pub(crate) fn moon_frame(&mut self, m: MoonId) {
        let now = self.now;
        let (vflow, vping, total) = (self[m].vflow, self[m].vping, self[m].total);
        if !self[DevId::Server].awake() || vflow == NO_FLOW || now - vping > PING_TIMEOUT {
            return;
        }
        let f = self[m].frames.len() as u32;
        let n = self[DevId::Client].links.len();
        let mut ideal = Vec::with_capacity(n);
        for i in 0..n {
            let l = self[DevId::Client].links[i];
            ideal.push(self.frame_time(l.down(), now, VIDEO_SIZE + INNER_HDR + OVERHEAD + IP_UDP, total));
        }
        self[m].frames.push(Frame { sent: now, ideal, ..Frame::default() });
        for i in 0..total {
            let aux = f << 8 | i as u32;
            self.at(now + i as Dur * SHARD_GAP, move |w| {
                let mo = &mut w[m];
                mo.vseq += 1;
                let (seq, flow, video) = (mo.vseq, mo.vflow, mo.video);
                let b = w.datagram(video, seq, aux, VIDEO_SIZE, now);
                w.send_on(DevId::Server, flow, b);
            });
        }
    }

    /// Sends one audio packet every 5 ms while the stream runs (Go's audio closure).
    fn moon_audio(&mut self, m: MoonId) {
        let now = self.now;
        let (aflow, vping) = (self[m].aflow, self[m].vping);
        if !self[DevId::Server].awake() || aflow == NO_FLOW || now - vping >= PING_TIMEOUT {
            return;
        }
        let owd = self.owds(false);
        let mo = &mut self[m];
        mo.audios.push(Sample { sent: now, got: 0, owd });
        let (audio, seq) = (mo.audio, (mo.audios.len() - 1) as u32);
        let b = self.datagram(audio, seq, 0, AUDIO_SIZE, now);
        self.send_on(DevId::Server, aflow, b);
    }

    /// Moonlight's frame assembly (RtpVideoQueue.c): a packet of a later frame purges every
    /// earlier incomplete frame; packets of earlier frames are rejected (Go video).
    pub(crate) fn moon_video(&mut self, m: MoonId, h: Hdr) {
        let now = self.now;
        let mo = &mut self[m];
        if mo.last_v >= 0 && now - mo.last_v > 20 * MS {
            mo.video_gaps.push([now, now - mo.last_v]);
        }
        mo.last_v = now;
        let f = h.aux >> 8;
        if f < mo.cur {
            return;
        }
        self.moon_purge(m, f);
        let mo = &mut self[m];
        let fr = &mut mo.frames[f as usize];
        if fr.done > 0 {
            return;
        }
        fr.got += 1;
        if usize::from(fr.got) == mo.data {
            fr.done = now;
            if now >= mo.freeze {
                mo.shown.push(now);
            }
        }
    }

    /// Marks frames before f that are incomplete as lost; each freezes video for one RTT (Go purge).
    pub(crate) fn moon_purge(&mut self, m: MoonId, f: u32) {
        while self[m].cur < f {
            let c = self[m].cur as usize;
            let fr = &self[m].frames[c];
            if fr.done == 0 && !fr.lost {
                let now = self.now;
                let rtt = self.rtt();
                let mo = &mut self[m];
                mo.frames[c].lost = true;
                mo.lost_at.push(now);
                mo.freeze = mo.freeze.max(now + rtt);
            }
            self[m].cur += 1;
        }
    }

    /// Purges the frames sent more than a second ago that never completed, at the end of a run
    /// (Go Settle).
    pub fn settle(&mut self, m: MoonId) {
        let (frames, recent) = (&self[m].frames, self.now - SEC);
        let mut f = frames.len();
        while f > 0 && frames[f - 1].sent > recent {
            f -= 1;
        }
        if f as u32 > self[m].cur {
            self.moon_purge(m, f as u32);
        }
    }

    /// Sends ENet packet i (a new one when first) and arms its resend timer (Go send).
    pub(crate) fn moon_send(&mut self, m: MoonId, i: usize, first: bool) {
        let now = self.now;
        if first {
            let owd = self.owds(true);
            self[m].inputs.push(Sample { sent: now, got: 0, owd });
            let mut rto = 2 * self.rtt();
            let mo = &mut self[m];
            if mo.rtt > 0 {
                rto = mo.rtt + 4 * mo.rtt_var;
            }
            mo.enet.push(EnetState { rto, ..EnetState::default() });
        }
        let mo = &mut self[m];
        let e = &mut mo.enet[i];
        e.tries += 1;
        let (tries, rto, input, sent) = (e.tries, e.rto, mo.input, mo.inputs[i].sent);
        let b = self.datagram(input, i as u32, tries, INPUT_SIZE, sent);
        self.send_from(DevId::Client, PORT_CONTROL, 50002, b);
        // The timer names the packet by index: an ack that arrives meanwhile is seen (Go's timer
        // held a pointer that append could leave behind; notes/rust.md).
        self.at(now + rto, move |w| {
            if !w[m].enet[i].acked && w[DevId::Client].awake() {
                w[m].enet[i].rto *= 2;
                w.moon_send(m, i, false);
            }
        });
    }

    /// The server's ENet: acknowledge every packet, apply in order (Go input).
    pub(crate) fn moon_input(&mut self, m: MoonId, f: FlowId, h: Hdr) {
        let (now, acks) = (self.now, self[m].acks);
        let b = self.datagram(acks, h.seq, 0, PING_SIZE, now);
        self.send_on(DevId::Server, f, b);
        let mo = &mut self[m];
        mo.enet[h.seq as usize].arrived = true;
        while mo.applied < mo.enet.len() && mo.enet[mo.applied].arrived {
            mo.inputs[mo.applied].got = now;
            mo.applied += 1;
        }
    }

    /// The client's ENet: RTT samples from first transmissions, as ENet's own estimator (Go ack).
    pub(crate) fn moon_ack(&mut self, m: MoonId, h: Hdr) {
        let now = self.now;
        let mo = &mut self[m];
        let i = h.seq as usize;
        let e = &mut mo.enet[i];
        if e.acked {
            return;
        }
        e.acked = true;
        if e.tries == 1 {
            let s = now - mo.inputs[i].sent;
            if mo.rtt == 0 {
                (mo.rtt, mo.rtt_var) = (s, s / 2);
            }
            let d = mo.rtt - s;
            mo.rtt_var += (d.abs() - mo.rtt_var) / 4;
            mo.rtt += (s - mo.rtt) / 8;
        }
    }

    /// Starts a saturating sender toward the client on port at bps from start until stop (Go Flood).
    pub fn flood(&mut self, port: u16, bps: f64, start: Dur, stop: Dur) -> FloodId {
        let fl = FloodId(self.apps.floods.len());
        self.apps.floods.push(Flood { bps, ..Flood::default() });
        let got = on_recv(move |w, _, _, _, b| {
            let i = (w.now / FLOOD_BUCKET) as usize;
            let got = &mut w[fl].got;
            if got.len() <= i {
                got.resize(i + 1, 0);
            }
            got[i] += b.len() as i64;
        });
        let s = self.stream("flood", port, true, got);
        self[fl].s = s;
        let ping = on_recv(move |w, _, f, _, _| w[fl].flow = f);
        let ping = self.stream("flood-ping", port, false, ping);
        self.every(start, PING_EVERY, move |w| {
            let b = w.datagram(ping, 0, 0, PING_SIZE, w.now);
            w.send_from(DevId::Client, port, 50010, b);
            w.now < stop
        });
        let size = VIDEO_SIZE - 16;
        let mut seq = 0;
        self.every(start, (size as f64 * 8.0 / bps * 1e9) as Dur, move |w| {
            let flow = w[fl].flow;
            if flow != NO_FLOW {
                seq += 1;
                let b = w.datagram(s, seq, 0, size, w.now);
                w.send_on(DevId::Server, flow, b);
            }
            w.now < stop
        });
        fl
    }

    /// Starts a bulk TCP sender toward the client on port from start until stop ([`Bulk`]).
    pub fn bulk(&mut self, port: u16, start: Dur, stop: Dur) -> BulkId {
        let b = BulkId(self.apps.bulks.len());
        self.apps.bulks.push(Bulk { cwnd: BULK_IW, ssthresh: BULK_MAX_CWND, ..Bulk::default() });
        // The client acks every tenth packet with the newest seq and the count so far.
        let data = on_recv(move |w, _, _, h, _| {
            let bk = &mut w[b];
            bk.got += 1;
            if bk.got % 10 == 0 {
                let (acks, got) = (bk.acks, bk.got);
                let d = w.datagram(acks, h.seq, got, PING_SIZE, w.now);
                w.send_from(DevId::Client, port, 50020, d);
            }
        });
        let s = self.stream("bulk", port, true, data);
        let acks = on_recv(move |w, _, f, h, _| {
            w.bulk_ack(b, f, h);
            bulk_fill(w, b, stop);
        });
        let acks = self.stream("bulk-ack", port, false, acks);
        (self[b].s, self[b].acks) = (s, acks);
        self.every(start, PING_EVERY, move |w| {
            let d = w.datagram(acks, 0, 0, PING_SIZE, w.now);
            w.send_from(DevId::Client, port, 50020, d);
            w.now < stop
        });
        // The retransmission timer: a silence of BULK_RTO with packets in flight loses them all.
        self.every(start, BULK_RTO / 4, move |w| {
            let now = w.now;
            let bk = &mut w[b];
            if bk.flow != NO_FLOW && bk.seq > bk.ack_seq && now - bk.last_ack >= BULK_RTO {
                bk.ssthresh = (bk.cwnd * BULK_BETA).max(2.0);
                (bk.cwnd, bk.ack_seq, bk.recover, bk.last_ack) = (BULK_IW, bk.seq, bk.seq, now);
                bk.holes = bk.ack_seq.saturating_sub(bk.ack_n);
                bulk_fill(w, b, stop);
            }
            now < stop
        });
        b
    }

    /// Starts an echo load (tests/system/load `udp` and `echo`): the client sends size-byte
    /// datagrams at bps from source port src to the server's port from start until stop, and the
    /// server echoes each one it receives on its flow at once. The echo carries the client's seq
    /// in its aux word and its own seq in the server's sending order, so each direction keeps the
    /// order invariant of its own flow.
    pub fn echo(&mut self, port: u16, src: u16, bps: f64, size: usize, start: Dur, stop: Dur) -> EchoId {
        let e = EchoId(self.apps.echoes.len());
        self.apps.echoes.push(Echo { bps, size, ..Echo::default() });
        let back = on_recv(move |w, _, _, h, _| {
            let now = w.now;
            if let Some(g) = (h.aux as usize).checked_sub(1).and_then(|i| w[e].got.get_mut(i))
                && *g == 0
            {
                *g = now;
            }
        });
        let back = self.stream("echo-back", port, true, back);
        let out = on_recv(move |w, _, f, h, b| {
            w[e].echoed += 1;
            let seq = w[e].echoed;
            let d = w.datagram(back, seq, h.seq, b.len(), h.sent);
            w.send_on(DevId::Server, f, d);
        });
        let out = self.stream("echo", port, true, out);
        (self[e].out, self[e].back) = (out, back);
        let mut seq = 0;
        self.every(start, (size as f64 * 8.0 / bps * 1e9) as Dur, move |w| {
            if w.now >= stop {
                return false;
            }
            seq += 1;
            let now = w.now;
            w[e].sent.push(now);
            w[e].got.push(0);
            let d = w.datagram(out, seq, 0, size, now);
            w.send_from(DevId::Client, port, src, d);
            true
        });
        e
    }

    /// The server's side of Bulk's acks: a ping (seq 0) opens the flow; an ack with news (the
    /// receiver's count grew) opens the window by what it acked, or cuts it on a new hole
    /// ([`Bulk`]). The caller then fills the window (`bulk_fill`).
    fn bulk_ack(&mut self, b: BulkId, f: FlowId, h: Hdr) {
        let now = self.now;
        let bk = &mut self[b];
        if h.seq == 0 {
            if bk.flow == NO_FLOW {
                bk.last_ack = now;
            }
            bk.flow = f;
            return;
        }
        if h.aux <= bk.ack_n {
            return; // acks ride copies on several lanes: a late one says nothing new
        }
        let acked = f64::from(h.aux - bk.ack_n);
        (bk.ack_seq, bk.ack_n, bk.last_ack) = (h.seq.max(bk.ack_seq), h.aux, now);
        let holes = bk.ack_seq.saturating_sub(bk.ack_n);
        if holes > bk.holes && bk.ack_seq > bk.recover {
            // A new loss outside the last recovery: cut once per window of data.
            bk.w_max = bk.cwnd;
            bk.cwnd = (bk.cwnd * BULK_BETA).max(2.0);
            (bk.ssthresh, bk.recover, bk.epoch) = (bk.cwnd, bk.seq, now);
        } else if bk.cwnd < bk.ssthresh {
            bk.cwnd += acked;
        } else {
            // CUBIC (RFC 9438): the window follows C (t − K)³ + W_max from the last cut, growing
            // by at least Reno's packet per window and at most half a packet per packet acked.
            let t = secs(now - bk.epoch);
            let k = (bk.w_max * (1.0 - BULK_BETA) / BULK_C).cbrt();
            let w = BULK_C * (t - k).powi(3) + bk.w_max;
            bk.cwnd += ((w - bk.cwnd) / bk.cwnd).clamp(1.0 / bk.cwnd, 0.5) * acked;
        }
        bk.holes = bk.holes.max(holes);
        bk.cwnd = bk.cwnd.min(BULK_MAX_CWND);
    }
}

/// Sends Bulk's next packets while its flow is open and fewer than `cwnd` are in flight past the
/// newest seq acked, until stop ([`Bulk`]).
fn bulk_fill(w: &mut World, b: BulkId, stop: Dur) {
    while w.now < stop {
        let bk = &mut w[b];
        if bk.flow == NO_FLOW || f64::from(bk.seq - bk.ack_seq) >= bk.cwnd.floor() {
            return;
        }
        bk.seq += 1;
        let (seq, flow, s) = (bk.seq, bk.flow, bk.s);
        let d = w.datagram(s, seq, 0, BULK_SIZE, w.now);
        w.send_on(DevId::Server, flow, d);
    }
}

/// Toggles the mouse between moving and resting for Exp(1 s) (Go's phase closure).
fn moon_phase(w: &mut World, m: MoonId, mut r: Rng) {
    w[m].moving = !w[m].moving;
    let t = w.now + (r.exp() * 1e9) as Dur;
    w.at(t, move |w| moon_phase(w, m, r));
}

impl Index<MoonId> for World {
    type Output = Moonlight;
    fn index(&self, m: MoonId) -> &Moonlight {
        &self.apps.moon[m.0]
    }
}

impl IndexMut<MoonId> for World {
    fn index_mut(&mut self, m: MoonId) -> &mut Moonlight {
        &mut self.apps.moon[m.0]
    }
}

impl Index<FloodId> for World {
    type Output = Flood;
    fn index(&self, f: FloodId) -> &Flood {
        &self.apps.floods[f.0]
    }
}

impl IndexMut<FloodId> for World {
    fn index_mut(&mut self, f: FloodId) -> &mut Flood {
        &mut self.apps.floods[f.0]
    }
}

impl Index<BulkId> for World {
    type Output = Bulk;
    fn index(&self, b: BulkId) -> &Bulk {
        &self.apps.bulks[b.0]
    }
}

impl IndexMut<BulkId> for World {
    fn index_mut(&mut self, b: BulkId) -> &mut Bulk {
        &mut self.apps.bulks[b.0]
    }
}

impl Index<EchoId> for World {
    type Output = Echo;
    fn index(&self, e: EchoId) -> &Echo {
        &self.apps.echoes[e.0]
    }
}

impl IndexMut<EchoId> for World {
    fn index_mut(&mut self, e: EchoId) -> &mut Echo {
        &mut self.apps.echoes[e.0]
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::fake::{Evil, fake_maker};

    /// The port of the test streams.
    const PORT: u16 = 9;

    fn world() -> World {
        World::new(1, fake_maker(false, Evil::None))
    }

    fn flow() -> FlowId {
        FlowId::new(PORT, 1)
    }

    /// Pairs the two devices' sessions: pairing 1 is the client's (client → server), pairing 2
    /// the server's.
    fn paired(w: &mut World) {
        let (c, s) = (w[DevId::Client].inst, w[DevId::Server].inst);
        w.pairs.extend([[c, s], [s, c]]);
        (w[DevId::Client].pair_id, w[DevId::Client].peer_inst) = (1, s);
        (w[DevId::Server].pair_id, w[DevId::Server].peer_inst) = (2, c);
    }

    /// A datagram of seq on s, submitted by the client now.
    fn sent(w: &mut World, s: StreamId, seq: u32) -> Vec<u8> {
        let b = w.datagram(s, seq, 0, 64, w.now);
        w.reg_submit(DevId::Client, &b);
        b
    }

    /// Submits b as d's and delivers it at d's peer at time at, in a handle call of its own.
    fn carry(w: &mut World, d: DevId, f: FlowId, b: &[u8], at: Dur) {
        w.reg_submit(d, b);
        (w.now, w.calls) = (at, w.calls + 1);
        w.reg_deliver(d.peer(), f, b);
    }

    #[test]
    fn datagram_layout_and_find() {
        let mut w = world();
        let a = w.stream("a", PORT, false, None);
        let s = w.stream("b", PORT, false, None);
        assert_eq!(w.datagram(a, 0, 0, 3, 0).len(), HDR_LEN);
        let b = w.datagram(s, 7, 9, 64, 1234);
        assert_eq!(b.len(), 64);
        assert_eq!(b[..8], [TAG_MAGIC, 1, 0, 0, 0, 0, 0, 1]);
        assert_eq!(b[8..12], 7u32.to_be_bytes());
        assert_eq!(b[12..20], 1234u64.to_be_bytes());
        assert_eq!(b[20..24], 9u32.to_be_bytes());
        assert!(b[HDR_LEN..].iter().all(|&x| x == 0));
        let h = Hdr { id: 1, seq: 7, sent: 1234, aux: 9 };
        assert_eq!(w.registry().find(&b), Some((1, s, h)));
        let g = w.reg.dg[1];
        assert_eq!((g.crc, g.n, g.st), (crc(&b), 64, s));
        assert_eq!((g.pair, g.ms, g.queued, g.done), (0, 0, false, false));
        // Too short, no magic, an id or a stream the registry never made.
        assert_eq!(w.registry().find(&b[..HDR_LEN - 1]), None);
        for (i, x) in [(0, 0xA4), (7, 2), (1, 2)] {
            let mut c = b.clone();
            c[i] = x;
            assert_eq!(w.registry().find(&c), None, "byte {i} = {x}");
        }
        assert_eq!(SHARD_GAP, 14_080);
    }

    #[test]
    fn submit_records_the_first_pairing() {
        let mut w = world();
        let s = w.stream("x", PORT, false, None);
        let b = w.datagram(s, 1, 0, 64, 0);
        (w[DevId::Client].pair_id, w.now) = (3, 1_500 * MS + 7);
        w.reg_submit(DevId::Client, &b);
        // A resend under another pairing is the same datagram; an untagged one is not known.
        (w[DevId::Client].pair_id, w.now) = (4, 2 * SEC);
        w.reg_submit(DevId::Client, &b);
        w.reg_submit(DevId::Client, &[0xA5; 10]);
        let g = w.reg.dg[0];
        assert_eq!((g.pair, g.ms, g.queued, g.done), (3, 1500, true, false));
        assert_eq!(w[s].sent, 1);
    }

    #[test]
    fn deliver_passes_each_datagram_to_its_stream_once() {
        let mut w = world();
        paired(&mut w);
        let got = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&got);
        let recv = on_recv(move |w, d, f, h, b| seen.borrow_mut().push((w.now, d, f, h, b.len())));
        let s = w.stream("x", PORT, false, recv);
        let hooked = Rc::new(RefCell::new(Vec::new()));
        let hook = Rc::clone(&hooked);
        w.hook(move |_, a| hook.borrow_mut().push((a.kind, a.dev, a.link, a.data.len())));
        let (b, c) = (sent(&mut w, s, 5), sent(&mut w, s, 6));
        w.now = 3 * MS;
        w.reg_deliver(DevId::Server, flow(), &b);
        w.reg_deliver(DevId::Server, flow(), &c);
        assert_eq!(w.failed(), None);
        let h = |id, seq| Hdr { id, seq, sent: 0, aux: 0 };
        let at = |id, seq| (3 * MS, DevId::Server, flow(), h(id, seq), 64);
        assert_eq!(*got.borrow(), [at(0, 5), at(1, 6)]);
        let delivered = (HookKind::Delivered, DevId::Server, None, 64);
        assert_eq!(*hooked.borrow(), [delivered; 2]);
        assert_eq!((w[s].sent, w[s].received, w.lost(s, 0, SEC)), (2, 2, 0));
        assert!(w.reg.dg.iter().all(|g| g.done));
        assert!(w[s].recv.is_some(), "the callback is put back");
    }

    /// The failure of delivering, at the server, what setup returns in a paired world, and how
    /// many datagrams the stream received.
    fn failure(setup: impl FnOnce(&mut World, StreamId) -> (FlowId, Vec<u8>)) -> (String, u64) {
        let mut w = world();
        paired(&mut w);
        let s = w.stream("x", PORT, false, None);
        let (f, b) = setup(&mut w, s);
        w.reg_deliver(DevId::Server, f, &b);
        (w.failed().unwrap_or_default().to_string(), w[s].received)
    }

    #[test]
    fn deliver_catches_forged_corrupted_unsent_twice_replayed_and_misrouted() {
        let (forged, _) = failure(|_, _| (flow(), vec![0x5A; 64]));
        let want = "at 0.000s: xanth: a forged datagram was delivered on flow 00090001 (64 bytes)";
        assert_eq!(forged, want);
        let (flipped, n) = failure(|w, s| {
            let mut b = sent(w, s, 1);
            b[30] ^= 1;
            (flow(), b)
        });
        assert_eq!(flipped, "at 0.000s: xanth: a corrupted datagram was delivered on x");
        assert_eq!(n, 0, "a failed delivery reaches no stream");
        let (cut, _) = failure(|w, s| (flow(), sent(w, s, 1)[..40].to_vec()));
        assert!(cut.contains("corrupted"), "{cut}");
        let (unsent, _) = failure(|w, s| (flow(), w.datagram(s, 1, 0, 64, 0)));
        assert!(unsent.ends_with("datagram 0 on x was delivered but never sent"), "{unsent}");
        let (twice, n) = failure(|w, s| {
            let b = sent(w, s, 1);
            w.reg_deliver(DevId::Server, flow(), &b);
            (flow(), b)
        });
        assert!(twice.ends_with("xanth: datagram 0 on x was delivered twice"), "{twice}");
        assert_eq!(n, 1);
        // The server restarted: its new process must not get the old pairing's datagram.
        let (replay, _) = failure(|w, s| {
            let b = sent(w, s, 1);
            w[DevId::Server].inst = 6;
            (flow(), b)
        });
        let want = "datagram 0 on x, sealed by an earlier session, was delivered (a replay)";
        assert!(replay.ends_with(want), "{replay}");
        let (port, _) = failure(|w, s| (FlowId::new(PORT + 1, 1), sent(w, s, 1)));
        assert!(port.ends_with("datagram for port 9 was delivered to port 10"), "{port}");
    }

    #[test]
    fn bulk_order_allows_one_late_packet_per_call() {
        let mut w = world();
        paired(&mut w);
        let s = w.stream("bulk", PORT, true, None);
        let b: Vec<_> = (0..12).map(|seq| sent(&mut w, s, seq)).collect();
        // 1–3 in order; 5 after a gap; 4 late, alone; 7 and 8; then 6 late with 9 after it.
        let calls: [&[usize]; 5] = [&[1, 2, 3], &[5], &[4], &[7, 8], &[6, 9]];
        for (call, seqs) in calls.into_iter().enumerate() {
            w.calls = call as u64 + 1;
            for &q in seqs {
                w.reg_deliver(DevId::Server, flow(), &b[q]);
            }
            assert_eq!(w.failed(), None, "call {}", call + 1);
        }
        w.calls += 1;
        w.reg_deliver(DevId::Server, flow(), &b[11]);
        w.reg_deliver(DevId::Server, flow(), &b[10]);
        let want = "xanth: bulk delivered seq 10 after 11 in one release";
        assert!(w.failed().unwrap().ends_with(want), "{:?}", w.failed());

        let mut w = world();
        paired(&mut w);
        let s = w.stream("bulk", PORT, true, None);
        let b: Vec<_> = (0..4).map(|seq| sent(&mut w, s, seq)).collect();
        w.calls = 1;
        w.reg_deliver(DevId::Server, flow(), &b[3]);
        w.calls = 2;
        w.reg_deliver(DevId::Server, flow(), &b[1]);
        w.reg_deliver(DevId::Server, flow(), &b[2]);
        let want = "xanth: bulk delivered two packets behind its newest in one Handle call";
        assert!(w.failed().unwrap().ends_with(want), "{:?}", w.failed());

        // A small stream may come in any order.
        let mut w = world();
        paired(&mut w);
        let s = w.stream("small", PORT, false, None);
        let b: Vec<_> = (0..4).map(|seq| sent(&mut w, s, seq)).collect();
        w.calls = 1;
        for q in [3, 1, 2] {
            w.reg_deliver(DevId::Server, flow(), &b[q]);
        }
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn lost_counts_sent_and_undelivered_datagrams_in_the_window() {
        let mut w = world();
        paired(&mut w);
        let (s, t) = (w.stream("x", PORT, false, None), w.stream("y", PORT, false, None));
        let mut at = |ms: Dur, st| {
            w.now = ms * MS;
            sent(&mut w, st, 0)
        };
        let delivered = at(100, s);
        at(200, s);
        at(300, s);
        at(200, t);
        w.datagram(s, 0, 0, 64, 0); // never submitted
        w.reg_deliver(DevId::Server, flow(), &delivered);
        assert_eq!(w.lost(s, 0, 300 * MS), 1);
        assert_eq!(w.lost(s, 0, 301 * MS), 2);
        assert_eq!(w.lost(s, 201 * MS, SEC), 1);
        assert_eq!(w.lost(t, 0, SEC), 1);
        assert_eq!(w.lost(s, -SEC, 0), 0);
    }

    #[test]
    fn moonlight_shards_streams_and_timers() {
        for (bps, fps, total, data) in [
            (30e6, 120.0, 23, 19),
            (60e6, 120.0, 45, 37),
            (90e6, 120.0, 68, 56),
            (20e6, 120.0, 15, 12),
            (4e6, 60.0, 6, 5),
        ] {
            let mut w = world();
            let m = w.moonlight(bps, fps, SEC, 2 * SEC);
            assert_eq!((w[m].total, w[m].data), (total, data), "{bps} bps at {fps} fps");
        }
        let mut w = world();
        let m = w.moonlight(30e6, 120.0, SEC, 2 * SEC);
        let streams: Vec<_> = w.reg.streams.iter().map(|s| (s.name, s.port, s.bulk)).collect();
        let want = [
            ("video", PORT_VIDEO, true),
            ("audio", PORT_AUDIO, false),
            ("input", PORT_CONTROL, false),
            ("enet-ack", PORT_CONTROL, false),
            ("ping", PORT_VIDEO, false),
            ("ping", PORT_AUDIO, false),
            ("ping", PORT_CONTROL, false),
        ];
        assert_eq!(streams, want);
        let mo = &w[m];
        let ids = [mo.video, mo.audio, mo.input, mo.acks];
        assert_eq!(ids, [0, 1, 2, 3].map(StreamId));
        assert_eq!(mo.pings, [4, 5, 6].map(StreamId));
        assert_eq!((mo.last_v, mo.moving, mo.vflow), (-1, false, NO_FLOW));
        // Three pings, frames, audio, the mouse and the input sender.
        assert_eq!(w.q.len(), 7);
    }

    #[test]
    fn pings_open_the_flows_and_audio_is_timed() {
        let mut w = world();
        paired(&mut w);
        let m = w.moonlight(30e6, 120.0, SEC, 2 * SEC);
        for (i, port) in [PORT_VIDEO, PORT_AUDIO, PORT_CONTROL].into_iter().enumerate() {
            let b = w.datagram(w[m].pings[i], 0, 0, PING_SIZE, w.now);
            carry(&mut w, DevId::Client, FlowId::new(port, 3), &b, (i as Dur + 1) * MS);
        }
        let mo = &w[m];
        let flows = (mo.vflow, mo.vping, mo.aflow, mo.cflow);
        let want = [PORT_VIDEO, PORT_AUDIO, PORT_CONTROL].map(|p| FlowId::new(p, 3));
        assert_eq!(flows, (want[0], MS, want[1], want[2]));
        w[m].audios.push(Sample::default());
        let b = w.datagram(w[m].audio, 0, 0, AUDIO_SIZE, 0);
        carry(&mut w, DevId::Server, FlowId::new(PORT_AUDIO, 3), &b, 40 * MS);
        assert_eq!(w[m].audios[0].got, 40 * MS);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn video_assembles_frames_and_records_gaps() {
        let mut w = world();
        paired(&mut w);
        let m = w.moonlight(30e6, 120.0, SEC, 2 * SEC);
        let (video, data) = (w[m].video, w[m].data as u32);
        let frame = |sent| Frame { sent, ..Frame::default() };
        w[m].frames.extend([frame(SEC), frame(SEC + 8 * MS), frame(1_500 * MS)]);
        let mut seq = 0;
        let mut shard = |w: &mut World, f: u32, i: u32, at: Dur| {
            seq += 1;
            let b = w.datagram(video, seq, f << 8 | i, VIDEO_SIZE, SEC);
            carry(w, DevId::Server, FlowId::new(PORT_VIDEO, 1), &b, at);
        };
        let t0 = SEC + 30 * MS;
        let at = |t: Dur, i: u32| t + Dur::from(i) * SHARD_GAP;
        // All data shards and two of parity.
        for i in 0..data + 2 {
            shard(&mut w, 0, i, at(t0, i));
        }
        let done = at(t0, data - 1);
        assert_eq!((w[m].frames[0].done, u32::from(w[m].frames[0].got)), (done, data));
        assert_eq!(w[m].shown, [done]);
        // Frame 1 starts 25 ms after the last video datagram: a gap, and frame 0 (decoded) is
        // passed without a loss. It completes while video is frozen, so it is not shown.
        let t1 = at(t0, data + 1) + 25 * MS;
        w[m].freeze = 10 * SEC;
        for i in 0..data {
            shard(&mut w, 1, i, at(t1, i));
        }
        let mo = &w[m];
        assert_eq!(mo.video_gaps, [[t1, 25 * MS]]);
        assert_eq!((mo.cur, mo.frames[1].done, mo.shown.len()), (1, at(t1, data - 1), 1));
        assert!(mo.lost_at.is_empty() && !mo.frames[0].lost);
        // A packet of a frame already passed is rejected.
        shard(&mut w, 0, 0, t1 + 500 * MS);
        assert_eq!(u32::from(w[m].frames[0].got), data);
        assert_eq!(w[m].last_v, t1 + 500 * MS);
        // Settle passes the decoded frames sent over a second ago and keeps the recent one.
        w.now = 2_200 * MS;
        w.settle(m);
        assert_eq!(w[m].cur, 2);
        w.now = 2_400 * MS;
        w.settle(m);
        assert_eq!(w[m].cur, 2, "frame 2 was sent less than a second ago");
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn enet_estimates_rtt_from_first_transmissions() {
        let mut w = world();
        let m = w.moonlight(30e6, 120.0, SEC, 2 * SEC);
        let input = |sent| Sample { sent, ..Sample::default() };
        let mo = &mut w[m];
        mo.inputs = vec![input(0), input(10 * MS), input(20 * MS)];
        mo.enet = [1, 1, 2].map(|tries| EnetState { tries, ..EnetState::default() }).to_vec();
        let mut ack = |seq, at| {
            w.now = at;
            w.moon_ack(m, Hdr { seq, ..Hdr::default() });
            (w[m].rtt, w[m].rtt_var)
        };
        // The first sample s = 40 ms: rtt s, variance s/2, then ENet's update.
        assert_eq!(ack(0, 40 * MS), (40 * MS, 15 * MS));
        // s = 50 ms: variance += (|40 − 50| − 15) / 4, rtt += (50 − 40) / 8.
        assert_eq!(ack(1, 60 * MS), (41_250_000, 13_750_000));
        // A second ack, and the ack of a resent packet, leave the estimate alone.
        assert_eq!(ack(1, 90 * MS), (41_250_000, 13_750_000));
        assert_eq!(ack(2, 300 * MS), (41_250_000, 13_750_000));
        assert!(w[m].enet.iter().all(|e| e.acked));
    }

    #[test]
    fn flood_counts_goodput_per_100_ms() {
        let mut w = world();
        paired(&mut w);
        let fl = w.flood(9000, 1e8, SEC, 2 * SEC);
        assert_eq!(w.q.len(), 2, "the pinger and the sender");
        let (s, f) = (w[fl].s, FlowId::new(9000, 1));
        assert_eq!((w[s].name, w[s].bulk, w[s].port), ("flood", true, 9000));
        let ping = w.datagram(StreamId(s.0 + 1), 0, 0, PING_SIZE, 0);
        carry(&mut w, DevId::Client, f, &ping, 0);
        assert_eq!(w[fl].flow, f);
        for (seq, at) in [(1, 50), (2, 150), (3, 160), (4, 450)] {
            let b = w.datagram(s, seq, 0, 1392, 0);
            carry(&mut w, DevId::Server, f, &b, at * MS);
        }
        assert_eq!(w[fl].got, [1392, 2784, 0, 0, 1392]);
        let goodput = |bytes: f64, secs: f64| bytes * 8.0 / secs;
        assert_eq!(w[fl].goodput(0, 200 * MS), goodput(4176.0, 0.2));
        assert_eq!(w[fl].goodput(100 * MS, SEC), goodput(4176.0, 0.9));
        assert_eq!(w[fl].goodput(SEC, 2 * SEC), 0.0);
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn bulk_opens_its_flow_counts_and_ignores_stale_acks() {
        let mut w = world();
        paired(&mut w);
        let b = w.bulk(9100, SEC, 2 * SEC);
        assert_eq!(w.q.len(), 2, "the pinger and the pacer");
        let (s, acks, f) = (w[b].s, w[b].acks, FlowId::new(9100, 1));
        assert_eq!((w[s].name, w[s].bulk), ("bulk", true));
        assert_eq!((w[acks].name, w[acks].bulk), ("bulk-ack", false));
        assert_eq!((w[b].cwnd, w[b].seq), (BULK_IW, 0));
        let ping = w.datagram(acks, 0, 0, PING_SIZE, 0);
        carry(&mut w, DevId::Client, f, &ping, 0);
        assert_eq!(w[b].flow, f);
        // Acks ride copies on several lanes: one that says nothing new changes nothing.
        w[b].ack_n = 20;
        let stale = w.datagram(acks, 25, 20, PING_SIZE, 0);
        carry(&mut w, DevId::Client, f, &stale, MS);
        assert_eq!((w[b].cwnd, w[b].ack_seq, w[b].ack_n), (BULK_IW, 0, 20));
        // The client counts what it gets (it acks every tenth, through the stand-in).
        for seq in 1..10 {
            let d = w.datagram(s, seq, 0, 1392, 0);
            carry(&mut w, DevId::Server, f, &d, 2 * MS);
        }
        assert_eq!((w[b].got, w[s].received), (9, 9));
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn echo_returns_each_datagram_on_its_flow_and_counts_loss() {
        let mut w = world();
        paired(&mut w);
        let e = w.echo(PORT, 40001, 1408.0 * 8e3, 1408, SEC, SEC + 10 * MS); // 1,000 a second
        assert_eq!(w.q.len(), 1, "the sender");
        let (out, back) = (w[e].out, w[e].back);
        assert_eq!((w[out].name, w[out].bulk, w[out].port), ("echo", true, PORT));
        assert_eq!((w[back].name, w[back].bulk, w[back].port), ("echo-back", true, PORT));
        w.run(2 * SEC).unwrap(); // no Session: the sender records what it hands the stand-in
        assert_eq!(w[e].sent, (0..10).map(|i| SEC + i * MS).collect::<Vec<_>>());
        assert_eq!(w[e].got, [0; 10]);
        // The server echoes the third datagram at once: a datagram of the back stream, as large.
        let f = FlowId::new(PORT, 1);
        let d = w.datagram(out, 3, 0, 1408, w[e].sent[2]);
        carry(&mut w, DevId::Client, f, &d, SEC + 40 * MS);
        assert_eq!(w[e].echoed, 1);
        let g = *w.reg.dg.last().unwrap();
        assert_eq!((g.st, usize::from(g.n)), (back, 1408));
        // Its echo (its own seq 1, the client's 3 in aux) reaches the client.
        let b = w.datagram(back, 1, 3, 1408, w[e].sent[2]);
        carry(&mut w, DevId::Server, f, &b, SEC + 80 * MS);
        assert_eq!(w[e].got[2], SEC + 80 * MS);
        assert_eq!(w[e].counts(SEC, SEC + 10 * MS, 2 * SEC), (10, 1));
        assert_eq!(w[e].counts(SEC, SEC + 10 * MS, SEC + 50 * MS), (10, 0));
        assert_eq!(w[e].loss(SEC, SEC + 10 * MS, 2 * SEC), 0.9);
        assert_eq!(w[e].loss(SEC + 2 * MS, SEC + 3 * MS, 2 * SEC), 0.0);
        assert_eq!(w[e].loss(0, SEC, 2 * SEC), 0.0, "nothing sent");
        assert_eq!(w.failed(), None);
    }

    #[test]
    fn seconds_as_go_counts_them() {
        assert_eq!(secs(1_500 * MS), 1.5);
        assert_eq!(secs(200 * MS), 0.2);
        assert_eq!(secs(-SEC), -1.0);
    }
}
