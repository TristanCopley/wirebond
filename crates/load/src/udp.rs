//! The paced UDP sender and the video-frame mode, both against an echo: every datagram is stamped,
//! the echo returns it, and the round trip is measured in this one process, so no clocks need to
//! agree.
//!
//! A datagram starts with a 32-byte stamp: magic "wbld", the run's id, its sequence number, its
//! send time in ns since the run began, and for video its frame, its shard and the shards the frame
//! needs. An echo from an earlier run, or anything else, is ignored.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::json::{Json, obj};
use crate::stats::{Meter, Proc, loadavg, quantiles, round3};

pub const MAGIC: u32 = 0x7762_6c64; // "wbld"
pub const STAMP: usize = 32;
/// How long the receiver waits for late echoes after the last send.
pub const GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stamp {
    pub run: u32,
    pub seq: u64,
    /// ns since the run's start.
    pub sent: u64,
    pub frame: u32,
    pub shard: u16,
    pub need: u16,
}

impl Stamp {
    pub fn write(&self, b: &mut [u8]) {
        b[0..4].copy_from_slice(&MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&self.run.to_be_bytes());
        b[8..16].copy_from_slice(&self.seq.to_be_bytes());
        b[16..24].copy_from_slice(&self.sent.to_be_bytes());
        b[24..28].copy_from_slice(&self.frame.to_be_bytes());
        b[28..30].copy_from_slice(&self.shard.to_be_bytes());
        b[30..32].copy_from_slice(&self.need.to_be_bytes());
    }

    /// The stamp of a datagram of run, if it is one.
    pub fn read(b: &[u8], run: u32) -> Option<Stamp> {
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap_or_default());
        let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap_or_default());
        let u16_at = |i: usize| u16::from_be_bytes(b[i..i + 2].try_into().unwrap_or_default());
        if b.len() < STAMP || u32_at(0) != MAGIC || u32_at(4) != run {
            return None;
        }
        Some(Stamp { run, seq: u64_at(8), sent: u64_at(16), frame: u32_at(24), shard: u16_at(28), need: u16_at(30) })
    }
}

/// One echoed datagram: its stamp and when it came back, in ns since the run's start.
#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub stamp: Stamp,
    pub at: u64,
}

/// A run id that differs between runs and processes.
fn run_id() -> u32 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let x = t ^ (u64::from(std::process::id()) << 32) ^ 0x9e37_79b9_7f4a_7c15;
    (x ^ (x >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9) as u32
}

/// A UDP socket connected to `to`, bound to `from` when given (as a source the harness forges).
pub fn connect(to: SocketAddr, from: Option<IpAddr>) -> io::Result<UdpSocket> {
    let local = from.unwrap_or(if to.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { Ipv6Addr::UNSPECIFIED.into() });
    let s = UdpSocket::bind(SocketAddr::new(local, 0))?;
    s.connect(to)?;
    Ok(s)
}

/// The run underway: the socket, the receiver thread and when it stops.
struct Run {
    sock: UdpSocket,
    id: u32,
    base: Instant,
    stop: Arc<AtomicU64>,
    rx: JoinHandle<Vec<Arrival>>,
}

impl Run {
    fn start(to: SocketAddr, from: Option<IpAddr>) -> io::Result<Run> {
        let sock = connect(to, from)?;
        let rsock = sock.try_clone()?;
        rsock.set_read_timeout(Some(Duration::from_millis(20)))?;
        let (id, base, stop) = (run_id(), Instant::now(), Arc::new(AtomicU64::new(u64::MAX)));
        let st = stop.clone();
        let rx = thread::spawn(move || {
            let mut arrivals = Vec::new();
            let mut b = vec![0u8; 65536];
            loop {
                let now = base.elapsed().as_nanos() as u64;
                if now >= st.load(Ordering::Relaxed) {
                    return arrivals;
                }
                // Errors: a timeout, or ICMP unreachable from an earlier datagram; keep reading.
                if let Ok(n) = rsock.recv(&mut b)
                    && let Some(stamp) = Stamp::read(&b[..n], id)
                {
                    arrivals.push(Arrival { stamp, at: base.elapsed().as_nanos() as u64 });
                }
            }
        });
        Ok(Run { sock, id, base, stop, rx })
    }

    fn now(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    /// Sends one stamped datagram; false if the send failed (no route, refused, a full buffer).
    fn send(&self, b: &mut [u8], mut s: Stamp) -> bool {
        s.run = self.id;
        s.sent = self.now();
        s.write(b);
        self.sock.send(b).is_ok()
    }

    /// Ends the run GRACE after now: the echoes received.
    fn finish(self) -> Vec<Arrival> {
        self.stop.store(self.now() + GRACE.as_nanos() as u64, Ordering::Relaxed);
        self.rx.join().unwrap_or_default()
    }
}

fn sleep_until(base: Instant, ns: u64) {
    let t = base + Duration::from_nanos(ns);
    let now = Instant::now();
    if t > now {
        thread::sleep(t - now);
    }
}

/// The paced sender's settings.
#[derive(Clone, Debug)]
pub struct UdpConfig {
    pub to: SocketAddr,
    pub from: Option<IpAddr>,
    pub mbps: f64,
    pub size: usize,
    pub duration: Duration,
    pub procs: Vec<Proc>,
}

/// The longest silence: from the first send to the first echo, between echoes, and from the last
/// echo to the end of sending; all of it with no echo.
pub fn longest_gap(first_send: u64, arrivals: &[u64], end: u64) -> u64 {
    let Some((&first, &last)) = arrivals.first().zip(arrivals.last()) else { return end.saturating_sub(first_send) };
    let between = arrivals.windows(2).map(|w| w[1].saturating_sub(w[0])).max().unwrap_or(0);
    between.max(first.saturating_sub(first_send)).max(end.saturating_sub(last))
}

/// Counts of a run's datagrams: received once, duplicates, and the round trips in ms.
fn tally(sent: usize, arrivals: &[Arrival]) -> (Vec<bool>, u64, Vec<f64>) {
    let mut got = vec![false; sent];
    let (mut dups, mut rtts) = (0, Vec::with_capacity(arrivals.len()));
    for a in arrivals {
        match got.get_mut(a.stamp.seq as usize) {
            Some(g) if *g => dups += 1,
            Some(g) => {
                *g = true;
                rtts.push(a.at.saturating_sub(a.stamp.sent) as f64 / 1e6);
            }
            None => {}
        }
    }
    (got, dups, rtts)
}

/// Sends size-byte datagrams at mbps for the duration and measures their echoes: loss, the round
/// trip's p50, p99, p99.9 and maximum, the longest gap, and the longest run of lost datagrams.
pub fn udp(c: &UdpConfig) -> io::Result<Json> {
    let size = c.size.max(STAMP);
    let interval = size as f64 * 8.0 / (c.mbps * 1e6) * 1e9; // ns per datagram
    let total = (c.duration.as_nanos() as f64 / interval).ceil().max(1.0) as u64;
    let meter = Meter::start(&c.procs);
    let run = Run::start(c.to, c.from)?;
    let mut b = vec![0u8; size];
    let (mut i, mut errors) = (0u64, 0u64);
    let mut sent_at = Vec::with_capacity(total as usize);
    let dur = c.duration.as_nanos() as u64;
    while i < total {
        let el = run.now();
        if el >= dur {
            break;
        }
        let due = ((el as f64 / interval) as u64 + 1).min(total);
        while i < due {
            sent_at.push(run.now());
            if !run.send(&mut b, Stamp { seq: i, ..Stamp::default() }) {
                errors += 1;
            }
            i += 1;
        }
        sleep_until(run.base, (i as f64 * interval) as u64);
    }
    let end = run.now();
    let arrivals = run.finish();
    let procs = meter.stop();
    let (got, dups, mut rtts) = tally(i as usize, &arrivals);
    let recv = rtts.len() as u64;
    let secs = (end as f64 / 1e9).max(1e-9);
    let mut times: Vec<u64> = arrivals.iter().map(|a| a.at).collect();
    times.sort_unstable();
    let gap = longest_gap(sent_at.first().copied().unwrap_or(0), &times, end);
    let lost_run = got.split(|&g| g).map(<[bool]>::len).max().unwrap_or(0);
    let [p50, p99, p999, max] = quantiles(&mut rtts);
    let mbps = |n: u64| round3(n as f64 * size as f64 * 8.0 / secs / 1e6);
    Ok(obj([
        ("to", c.to.to_string().into()),
        ("size", size.into()),
        ("target_mbps", c.mbps.into()),
        ("sent_mbps", mbps(i).into()),
        ("recv_mbps", mbps(recv).into()),
        ("secs", round3(secs).into()),
        ("sent", i.into()),
        ("recv", recv.into()),
        ("lost", (i - recv).into()),
        ("dups", dups.into()),
        ("send_errors", errors.into()),
        ("loss_pct", round3((i - recv) as f64 / i.max(1) as f64 * 100.0).into()),
        ("p50_ms", p50.into()),
        ("p99_ms", p99.into()),
        ("p999_ms", p999.into()),
        ("max_ms", max.into()),
        ("gap_ms", round3(gap as f64 / 1e6).into()),
        ("lost_run_ms", round3(lost_run as f64 * interval / 1e6).into()),
        ("limited", ((i as f64) < 0.97 * dur as f64 / interval).into()),
        ("procs", procs),
        ("load", loadavg().into()),
    ]))
}

/// The video mode's settings: Sunshine's frames as Moonlight receives them.
#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub to: SocketAddr,
    pub from: Option<IpAddr>,
    /// The video's bitrate; parity comes on top.
    pub mbps: f64,
    pub fps: f64,
    pub size: usize,
    /// Reed–Solomon parity as a percentage of a frame's data shards (Sunshine's default: 20).
    pub fec: f64,
    pub duration: Duration,
    pub procs: Vec<Proc>,
}

/// Data and parity shards per frame.
pub fn shards(c: &VideoConfig) -> (u16, u16) {
    let size = c.size.max(STAMP) as f64;
    let data = (c.mbps * 1e6 / 8.0 / c.fps / size).ceil().clamp(1.0, 4096.0);
    let parity = (data * c.fec / 100.0).ceil().clamp(0.0, 4096.0);
    (data as u16, parity as u16)
}

/// Moonlight's frame assembly (RtpVideoQueue): a frame is decodable once `need` of its shards have
/// arrived; a shard of a later frame purges every earlier frame still incomplete, and a shard of a
/// frame already passed is rejected (late).
#[derive(Debug, Default)]
pub struct Frames {
    need: Vec<u16>,
    got: Vec<u16>,
    /// When each frame became decodable, ns since the run's start.
    pub done: Vec<Option<u64>>,
    pub purged: Vec<bool>,
    cur: usize,
    pub late: u64,
}

impl Frames {
    pub fn new(frames: usize, need: u16) -> Frames {
        Frames {
            need: vec![need; frames],
            got: vec![0; frames],
            done: vec![None; frames],
            purged: vec![false; frames],
            ..Frames::default()
        }
    }

    /// One shard of frame f arrived at `at` (each shard once: duplicates go first).
    pub fn arrive(&mut self, f: usize, at: u64) {
        if f >= self.got.len() {
            return;
        }
        if f < self.cur {
            if self.done[f].is_none() {
                self.late += 1;
            }
            return;
        }
        for c in self.cur..f {
            if self.done[c].is_none() {
                self.purged[c] = true;
            }
        }
        self.cur = f;
        self.got[f] += 1;
        if self.got[f] == self.need[f] {
            self.done[f] = Some(at);
        }
    }
}

/// Sends frames of mbps / fps bytes in size-byte shards, plus parity, each frame a burst at its
/// frame time, and assembles the echoes as Moonlight would: decodable and unrecovered frames, and
/// the frame delay (from the frame's send to its last needed shard) at p50, p99, p99.9 and worst.
pub fn video(c: &VideoConfig) -> io::Result<Json> {
    let size = c.size.max(STAMP);
    let (data, parity) = shards(c);
    let per = usize::from(data) + usize::from(parity);
    let frame_ns = 1e9 / c.fps;
    let total = ((c.duration.as_nanos() as f64 / frame_ns).ceil() as usize).max(1);
    let meter = Meter::start(&c.procs);
    let run = Run::start(c.to, c.from)?;
    let mut b = vec![0u8; size];
    let (mut seq, mut errors) = (0u64, 0u64);
    let mut starts = Vec::with_capacity(total);
    for f in 0..total {
        sleep_until(run.base, (f as f64 * frame_ns) as u64);
        starts.push(run.now());
        for shard in 0..per {
            let s = Stamp { seq, frame: f as u32, shard: shard as u16, need: data, ..Stamp::default() };
            if !run.send(&mut b, s) {
                errors += 1;
            }
            seq += 1;
        }
    }
    let end = run.now();
    let arrivals = run.finish();
    let procs = meter.stop();
    let (got, dups, _) = tally(seq as usize, &arrivals);
    let recv = got.iter().filter(|&&g| g).count() as u64;
    let mut frames = Frames::new(total, data);
    let mut seen = vec![false; seq as usize];
    for a in &arrivals {
        if let Some(s) = seen.get_mut(a.stamp.seq as usize)
            && !*s
        {
            *s = true;
            frames.arrive(a.stamp.frame as usize, a.at);
        }
    }
    let mut delays: Vec<f64> =
        frames.done.iter().zip(&starts).filter_map(|(d, &s)| d.map(|d| d.saturating_sub(s) as f64 / 1e6)).collect();
    let decodable = delays.len();
    let mut done_at: Vec<u64> = frames.done.iter().flatten().copied().collect();
    done_at.sort_unstable();
    let gap = longest_gap(starts.first().copied().unwrap_or(0), &done_at, end);
    let [p50, p99, p999, max] = quantiles(&mut delays);
    let secs = (end as f64 / 1e9).max(1e-9);
    Ok(obj([
        ("to", c.to.to_string().into()),
        ("mbps", c.mbps.into()),
        ("fps", c.fps.into()),
        ("size", size.into()),
        ("fec_pct", c.fec.into()),
        ("data_shards", data.into()),
        ("parity_shards", parity.into()),
        ("secs", round3(secs).into()),
        ("sent_mbps", round3(seq as f64 * size as f64 * 8.0 / secs / 1e6).into()),
        ("frames", total.into()),
        ("decodable", decodable.into()),
        ("unrecovered", (total - decodable).into()),
        ("purged", frames.purged.iter().filter(|&&p| p).count().into()),
        ("late_shards", frames.late.into()),
        ("sent", seq.into()),
        ("recv", recv.into()),
        ("dups", dups.into()),
        ("send_errors", errors.into()),
        ("loss_pct", round3((seq - recv) as f64 / seq.max(1) as f64 * 100.0).into()),
        ("frame_p50_ms", p50.into()),
        ("frame_p99_ms", p99.into()),
        ("frame_p999_ms", p999.into()),
        ("frame_max_ms", max.into()),
        // The longest time without a frame becoming decodable: a freeze.
        ("gap_ms", round3(gap as f64 / 1e6).into()),
        ("procs", procs),
        ("load", loadavg().into()),
    ]))
}
