//! Control channels (SPEC 2.0 §16): TCP on the mesh addresses, one JSON line per message. Each
//! channel has a reader thread, which decodes lines (at most MAX_MSG) and hands them to the loop,
//! and a writer thread fed through a queue of CTL_QUEUE messages: a sender that would queue more
//! closes the channel. Dialing and accepting run on their own threads too, so the loop never
//! blocks. Keepalive: KEEP_IDLE, KEEP_INTERVAL, KEEP_COUNT. Owner: node daemon.
//!
//! Order: a channel's `Ev::Up` is queued before its reader thread starts, so the loop knows the
//! channel before any of its lines (a peer's hello read first was dropped, and the bond never
//! opened). A channel dialed for a bond that brings no `bond ok` within BOND_OK_WAIT is closed and
//! the anchor dialed again after the backoff (§17).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use socket2::{Domain, SockRef, Socket, TcpKeepalive, Type};
use wirebond_core::{CTL_QUEUE, KEEP_COUNT, KEEP_IDLE, KEEP_INTERVAL, MAX_MSG, Mono, SEC};

use super::proto::{self, ErrorCode, Msg};
use super::{Daemon, Purpose};
use crate::host::Waker;

/// How long a dial may take to connect.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a channel dialed for the client's bond may stay up without `bond ok`.
pub const BOND_OK_WAIT: Mono = 5 * SEC;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh channel id.
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// What the channel threads tell the loop.
pub enum Ev {
    /// A channel is open: dialed by us (`dialed`) or accepted.
    Up { chan: Chan, dialed: bool },
    /// A dial failed: the name did not resolve or nothing answered.
    DialFailed { id: u64, err: String },
    /// One received line: a message, or why it is not one.
    Line { id: u64, msg: Result<Msg, ErrorCode> },
    /// The channel closed.
    Down { id: u64 },
    /// A name resolved for the allow-list (§10.1): its addresses.
    Resolved { name: String, addrs: Vec<IpAddr> },
    /// The captive-portal check of the link at `addr` (§17): None when nothing answered.
    Captive { addr: IpAddr, captive: Option<bool> },
    /// This node's egress, measured for the `measure start` on channel `id` (§18 step 4).
    Egress { id: u64, ms: f64 },
}

/// One open channel, as the loop holds it.
pub struct Chan {
    pub id: u64,
    pub peer: SocketAddr,
    /// When the channel connected.
    pub opened: Instant,
    tx: SyncSender<Vec<u8>>,
    stream: TcpStream,
}

impl Chan {
    /// Queues `m`. False, and the channel closed, when CTL_QUEUE messages already wait or `m`
    /// exceeds MAX_MSG (§16).
    pub fn send(&self, m: &Msg) -> bool {
        let ok = match proto::encode(m) {
            Some(line) => !matches!(self.tx.try_send(line), Err(TrySendError::Full(_) | TrySendError::Disconnected(_))),
            None => false,
        };
        if !ok {
            self.abort();
        }
        ok
    }

    /// Closes the channel once what is queued is written: an `error` or `bond close` goes first.
    pub fn close(&self) {
        if self.tx.try_send(Vec::new()).is_err() {
            self.abort();
        }
    }

    /// Closes the channel now, dropping what is queued.
    pub fn abort(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// Wraps a connected stream: keepalive, the writer thread, `Ev::Up`, then the reader thread, so
/// `Ev::Up` reaches the loop before any `Ev::Line` of the channel.
fn start(stream: TcpStream, id: u64, dialed: bool, tx: &Sender<Ev>, waker: &Arc<Waker>) -> io::Result<()> {
    let peer = stream.peer_addr()?;
    let ns = |m: i64| Duration::from_nanos(m as u64);
    let ka = TcpKeepalive::new().with_time(ns(KEEP_IDLE)).with_interval(ns(KEEP_INTERVAL)).with_retries(KEEP_COUNT);
    let _ = SockRef::from(&stream).set_tcp_keepalive(&ka);
    let _ = stream.set_nodelay(true);
    let (qtx, qrx) = mpsc::sync_channel::<Vec<u8>>(CTL_QUEUE);
    let mut w = stream.try_clone()?;
    thread::spawn(move || {
        for line in qrx {
            // An empty line is `close`: everything before it is written.
            if line.is_empty() || w.write_all(&line).is_err() {
                break;
            }
        }
        let _ = w.shutdown(Shutdown::Both);
    });
    let r = stream.try_clone()?;
    let _ = tx.send(Ev::Up { chan: Chan { id, peer, opened: Instant::now(), tx: qtx, stream }, dialed });
    waker.wake();
    let (rtx, rwaker) = (tx.clone(), waker.clone());
    thread::spawn(move || read_lines(r, id, &rtx, &rwaker));
    Ok(())
}

/// The reader: one `Ev::Line` per line, then `Ev::Down`. A line past MAX_MSG ends the channel.
fn read_lines(s: TcpStream, id: u64, tx: &Sender<Ev>, waker: &Waker) {
    let mut r = BufReader::new(s);
    loop {
        let mut line = Vec::new();
        match (&mut r).take(MAX_MSG as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            break; // too long, or cut off
        }
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if tx.send(Ev::Line { id, msg: proto::decode(&line) }).is_err() {
            return;
        }
        waker.wake();
    }
    let _ = r.get_ref().shutdown(Shutdown::Both);
    let _ = tx.send(Ev::Down { id });
    waker.wake();
}

/// Accepts channels on `addr` for as long as the listener works.
pub fn listen(addr: SocketAddr, tx: Sender<Ev>, waker: Arc<Waker>) -> io::Result<()> {
    let ln = TcpListener::bind(addr)?;
    thread::spawn(move || {
        while let Ok((s, _)) = ln.accept() {
            if start(s, next_id(), false, &tx, &waker).is_err() {
                continue;
            }
        }
    });
    Ok(())
}

/// Dials `host` (a name through the system resolver, or a literal address) on `port`, from `from`
/// when given (this node's mesh address), as channel `id`.
pub fn dial(host: String, port: u16, from: Option<IpAddr>, id: u64, tx: Sender<Ev>, waker: Arc<Waker>) {
    thread::spawn(move || {
        let res = (|| -> io::Result<TcpStream> {
            let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
                Ok(ip) => vec![SocketAddr::new(ip, port)],
                Err(_) => (host.as_str(), port).to_socket_addrs()?.collect(),
            };
            let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address"));
            for to in addrs {
                let s = Socket::new(Domain::for_address(to), Type::STREAM, None)?;
                if let Some(f) = from.filter(|f| f.is_ipv4() == to.is_ipv4()) {
                    let _ = s.bind(&SocketAddr::new(f, 0).into());
                }
                match s.connect_timeout(&to.into(), DIAL_TIMEOUT) {
                    Ok(()) => return Ok(s.into()),
                    Err(e) => last = e,
                }
            }
            Err(last)
        })();
        match res {
            Ok(s) => {
                if let Err(e) = start(s, id, true, &tx, &waker) {
                    let _ = tx.send(Ev::DialFailed { id, err: e.to_string() });
                    waker.wake();
                }
            }
            Err(e) => {
                let _ = tx.send(Ev::DialFailed { id, err: e.to_string() });
                waker.wake();
            }
        }
    });
}

/// What the egress measurement reports when no reference answered: it scores far behind.
pub const NO_EGRESS_MS: f64 = 10_000.0;

/// Measures this node's egress off the loop (§18 step 4): the median TCP connect time over
/// PICK_EGRESS_TRIES attempts to each of PICK_REFERENCES, for channel `id`. The references are
/// tried in parallel, each attempt for at most EGRESS_TRY, and the answer comes within `within`
/// whatever answers: a reference that does not answer only gives fewer samples.
pub fn egress(id: u64, within: Duration, tx: Sender<Ev>, waker: Arc<Waker>) {
    thread::spawn(move || {
        let end = Instant::now() + within;
        let (stx, srx) = mpsc::channel::<f64>();
        for r in wirebond_core::PICK_REFERENCES {
            let Ok(to) = r.parse::<SocketAddr>() else { continue };
            let stx = stx.clone();
            thread::spawn(move || {
                for _ in 0..wirebond_core::PICK_EGRESS_TRIES {
                    let left = end.saturating_duration_since(Instant::now()).min(EGRESS_TRY);
                    if left.is_zero() {
                        return;
                    }
                    let t = Instant::now();
                    if TcpStream::connect_timeout(&to, left).is_ok()
                        && stx.send(t.elapsed().as_secs_f64() * 1e3).is_err()
                    {
                        return;
                    }
                }
            });
        }
        drop(stx);
        let mut ms = Vec::new();
        while let Ok(m) = srx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            ms.push(m);
        }
        let _ = tx.send(Ev::Egress { id, ms: median(&mut ms) });
        waker.wake();
    });
}

/// One egress attempt's longest wait.
const EGRESS_TRY: Duration = Duration::from_secs(1);

/// The median of `ms`; NO_EGRESS_MS when empty.
fn median(ms: &mut [f64]) -> f64 {
    ms.sort_by(f64::total_cmp);
    ms.get(ms.len() / 2).copied().unwrap_or(NO_EGRESS_MS)
}

/// Resolves `name` through the system resolver for the allow-list, off the loop.
pub fn resolve(name: String, tx: Sender<Ev>, waker: Arc<Waker>) {
    thread::spawn(move || {
        let addrs = (name.as_str(), 0).to_socket_addrs().map(|a| a.map(|a| a.ip()).collect()).unwrap_or_default();
        let _ = tx.send(Ev::Resolved { name, addrs });
        waker.wake();
    });
}

impl Daemon {
    /// A channel dialed for the client's bond that has not brought `bond ok` within BOND_OK_WAIT
    /// is closed, and the anchor dialed again after the backoff (§17), as an unreachable one.
    pub(super) fn chan_tick(&mut self) {
        let wait = Duration::from_nanos(BOND_OK_WAIT as u64);
        let stuck: Vec<u64> = self
            .chans
            .iter()
            .filter(|(id, p)| {
                matches!(p.purpose, Some(Purpose::Bond))
                    && !self.bonds.iter().any(|b| b.chan == Some(**id))
                    && p.chan.opened.elapsed() >= wait
            })
            .map(|(&id, _)| id)
            .collect();
        for id in stuck {
            let Some(p) = self.chans.get_mut(&id) else { continue };
            p.purpose = None;
            p.chan.abort();
            let why = format!("no bond ok within {} s from {}", BOND_OK_WAIT / SEC, p.chan.peer);
            self.log(format!("channel {id}: {why}: redialing"));
            self.dial_failed(why);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// A peer that writes its line at once is never read before the channel is up (the stalled
    /// bond of the harness: the anchor's hello came first and was dropped).
    #[test]
    fn up_comes_before_any_line() {
        let ip = IpAddr::V4(Ipv4Addr::new(127, 46, 0, 1));
        for _ in 0..50 {
            let ln = TcpListener::bind((ip, 0)).unwrap();
            let at = ln.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut s, _) = ln.accept().unwrap();
                s.write_all(b"{\"t\":\"view\"}\n").unwrap();
                s
            });
            let (tx, rx) = mpsc::channel();
            dial(ip.to_string(), at.port(), None, 9, tx, Arc::new(Waker::new().unwrap()));
            let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(first, Ev::Up { dialed: true, .. }), "the channel first");
            let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(second, Ev::Line { id: 9, msg: Ok(Msg::View(_)) }), "then its line");
            drop(server.join().unwrap());
        }
    }

    /// The answer comes within its bound whatever the references do: inside `unshare -n` none
    /// answers (NO_EGRESS_MS); with the internet, a median of what answered within it.
    #[test]
    fn egress_answers_within_its_bound() {
        let (tx, rx) = mpsc::channel();
        let t = Instant::now();
        egress(3, Duration::from_millis(300), tx, Arc::new(Waker::new().unwrap()));
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Ev::Egress { id, ms } => assert!(id == 3 && (ms == NO_EGRESS_MS || ms <= 300.0), "{id} {ms}"),
            _ => panic!("not egress"),
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert_eq!(median(&mut [5.0, 1.0, 3.0]), 3.0);
    }
}
