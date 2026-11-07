//! The anchor's DNS relay (SPEC 2.0 §7): UDP and TCP port 53 on the anchor's bond addresses, of
//! the families its egress achieved (the caller passes those); an address whose family the host
//! does not support (EAFNOSUPPORT) is skipped, and one address failing never stops the others.
//! Each query from a live bond's client address goes verbatim to the anchor's own first resolver,
//! and each answer goes back to the client that asked: over UDP from that bond's own upstream
//! socket, read by the loop; over TCP on a thread per connection, each query on its own upstream
//! connection. It reads only the 2-byte query ID (and TCP's 2-byte length) and caches nothing; at
//! most DNS_INFLIGHT queries per bond, UDP and TCP together, each dropped after DNS_TIMEOUT.
//! Owner: node daemon.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use wirebond_core::{DNS_INFLIGHT, DNS_TIMEOUT, Mono};

/// Datagrams read from one socket per pump at most.
const BURST: usize = 256;
/// EAFNOSUPPORT: the host has no such address family (an IPv4-only machine's IPv6).
#[cfg(unix)]
const EAFNOSUPPORT: i32 = libc::EAFNOSUPPORT;
#[cfg(windows)]
const EAFNOSUPPORT: i32 = 10047; // WSAEAFNOSUPPORT

/// One query in flight: who asked, on which listening socket, and when.
struct Query {
    from: SocketAddr,
    sock: usize,
    at: Mono,
}

/// One bond's upstream socket and its queries by ID.
struct Upstream {
    sock: UdpSocket,
    inflight: HashMap<u16, Query>,
}

/// What the TCP threads share with the loop.
#[derive(Default)]
struct Shared {
    /// The live bonds' client addresses, as the last pump saw them.
    live: Mutex<Vec<IpAddr>>,
    /// Queries in flight per client address: over TCP, and over UDP as the last pump counted.
    tcp: Mutex<HashMap<IpAddr, usize>>,
    udp: Mutex<HashMap<IpAddr, usize>>,
    stop: AtomicBool,
}

impl Shared {
    fn live(&self, a: IpAddr) -> bool {
        self.live.lock().unwrap_or_else(PoisonError::into_inner).contains(&a)
    }

    /// Takes one of `a`'s DNS_INFLIGHT places for a TCP query; false when none is left.
    fn take(&self, a: IpAddr) -> bool {
        let udp = self.udp.lock().unwrap_or_else(PoisonError::into_inner).get(&a).copied().unwrap_or(0);
        let mut tcp = self.tcp.lock().unwrap_or_else(PoisonError::into_inner);
        let n = tcp.entry(a).or_insert(0);
        if *n + udp >= DNS_INFLIGHT {
            return false;
        }
        *n += 1;
        true
    }

    fn give(&self, a: IpAddr) {
        let mut tcp = self.tcp.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = tcp.get_mut(&a) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                tcp.remove(&a);
            }
        }
    }

    fn tcp_inflight(&self, a: IpAddr) -> usize {
        self.tcp.lock().unwrap_or_else(PoisonError::into_inner).get(&a).copied().unwrap_or(0)
    }
}

pub struct Relay {
    /// The listening sockets, on the anchor's bond addresses.
    socks: Vec<UdpSocket>,
    upstream: SocketAddr,
    /// Per client bond address.
    bonds: HashMap<IpAddr, Upstream>,
    /// The TCP listeners' addresses, which `drop` wakes.
    tcp: Vec<SocketAddr>,
    shared: Arc<Shared>,
}

impl Relay {
    /// Listens on UDP and TCP `port` of each of `addrs` and relays to `upstream`. An address
    /// whose family the host lacks is skipped, and one that fails otherwise is reported with its
    /// error and skipped too; it fails only when no address could listen at all.
    pub fn new(addrs: &[IpAddr], port: u16, upstream: SocketAddr) -> io::Result<(Relay, Vec<(IpAddr, io::Error)>)> {
        let shared = Arc::new(Shared::default());
        let (mut socks, mut tcp, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
        let mut none = io::Error::new(io::ErrorKind::AddrNotAvailable, "no bond address to listen on");
        for &a in addrs {
            let udp = UdpSocket::bind((a, port)).and_then(|s| s.set_nonblocking(true).map(|()| s));
            match udp {
                Ok(s) => socks.push(s),
                Err(e) if e.raw_os_error() == Some(EAFNOSUPPORT) => {
                    none = e;
                    continue;
                }
                Err(e) => {
                    skipped.push((a, e));
                    continue;
                }
            }
            // TCP on the port UDP took (a test's port 0 is the kernel's pick).
            let at = socks.last().and_then(|s| s.local_addr().ok()).map_or(port, |l| l.port());
            match TcpListener::bind((a, at)) {
                Ok(ln) => {
                    tcp.push(ln.local_addr()?);
                    let (shared, upstream) = (shared.clone(), upstream);
                    thread::spawn(move || accept(ln, upstream, shared));
                }
                Err(e) => skipped.push((a, e)),
            }
        }
        if socks.is_empty() {
            return Err(skipped.pop().map_or(none, |(_, e)| e));
        }
        Ok((Relay { socks, upstream, bonds: HashMap::new(), tcp, shared }, skipped))
    }

    /// The sockets the loop waits on: the listeners and every bond's upstream socket.
    pub fn sockets(&self) -> impl Iterator<Item = &UdpSocket> {
        self.socks.iter().chain(self.bonds.values().map(|u| &u.sock))
    }

    /// Relays what waits: queries from `live` addresses (the live bonds' client addresses), and
    /// the answers. Drops queries past DNS_TIMEOUT and the state of bonds no longer live.
    pub fn pump(&mut self, now: Mono, live: &[IpAddr]) {
        let mut buf = [0u8; 4096];
        {
            let mut l = self.shared.live.lock().unwrap_or_else(PoisonError::into_inner);
            if *l != live {
                *l = live.to_vec();
            }
        }
        let live = |a: IpAddr| live.contains(&a);
        self.bonds.retain(|a, _| live(*a));
        for (i, s) in self.socks.iter().enumerate() {
            for _ in 0..BURST {
                let Ok((n, from)) = s.recv_from(&mut buf) else { break };
                let client = from.ip().to_canonical();
                if n < 12 || !live(client) {
                    continue;
                }
                let up = match self.bonds.entry(client) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let any: IpAddr = if self.upstream.is_ipv4() {
                            std::net::Ipv4Addr::UNSPECIFIED.into()
                        } else {
                            std::net::Ipv6Addr::UNSPECIFIED.into()
                        };
                        let Ok(sock) = UdpSocket::bind((any, 0)) else { continue };
                        if sock.set_nonblocking(true).is_err() {
                            continue;
                        }
                        e.insert(Upstream { sock, inflight: HashMap::new() })
                    }
                };
                up.inflight.retain(|_, q| now - q.at < DNS_TIMEOUT);
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                let tcp = self.shared.tcp_inflight(client);
                if up.inflight.len() + tcp >= DNS_INFLIGHT && !up.inflight.contains_key(&id) {
                    continue;
                }
                if up.sock.send_to(&buf[..n], self.upstream).is_ok() {
                    up.inflight.insert(id, Query { from, sock: i, at: now });
                }
            }
        }
        for up in self.bonds.values_mut() {
            for _ in 0..BURST {
                let Ok((n, from)) = up.sock.recv_from(&mut buf) else { break };
                if from != self.upstream || n < 12 {
                    continue;
                }
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                if let Some(q) = up.inflight.remove(&id).filter(|q| now - q.at < DNS_TIMEOUT)
                    && let Some(s) = self.socks.get(q.sock)
                {
                    let _ = s.send_to(&buf[..n], q.from);
                }
            }
        }
        let counts: HashMap<IpAddr, usize> =
            self.bonds.iter().filter(|(_, u)| !u.inflight.is_empty()).map(|(&a, u)| (a, u.inflight.len())).collect();
        let mut udp = self.shared.udp.lock().unwrap_or_else(PoisonError::into_inner);
        if *udp != counts {
            *udp = counts;
        }
    }
}

impl Drop for Relay {
    /// Stops the TCP side: each accept thread is woken by one last connection and ends.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for &a in &self.tcp {
            let _ = TcpStream::connect_timeout(&a, Duration::from_millis(200));
        }
    }
}

/// One TCP listener: a thread per connection from a live bond's client address.
fn accept(ln: TcpListener, upstream: SocketAddr, shared: Arc<Shared>) {
    while let Ok((s, from)) = ln.accept() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let client = from.ip().to_canonical();
        if !shared.live(client) {
            continue;
        }
        let shared = shared.clone();
        thread::spawn(move || serve(s, client, upstream, &shared));
    }
}

/// One client connection (RFC 7766 framing: a 2-byte length, then the message): each query is
/// relayed verbatim on its own upstream connection, its answer written back, while the client
/// stays live, within DNS_TIMEOUT per query; a connection idle DNS_TIMEOUT closes.
fn serve(mut s: TcpStream, client: IpAddr, upstream: SocketAddr, shared: &Shared) {
    let limit = Duration::from_nanos(DNS_TIMEOUT as u64);
    let _ = s.set_read_timeout(Some(limit));
    let _ = s.set_write_timeout(Some(limit));
    while let Some(q) = read_msg(&mut s) {
        if shared.stop.load(Ordering::SeqCst) || !shared.live(client) {
            break;
        }
        if q.len() < 12 || !shared.take(client) {
            continue; // dropped, as UDP drops it: the client times out on it
        }
        let answer = ask(&q, upstream, Instant::now() + limit);
        shared.give(client);
        if let Some(a) = answer
            && write_msg(&mut s, &a).is_err()
        {
            break;
        }
    }
    let _ = s.shutdown(Shutdown::Both);
}

/// One query over TCP to `upstream`, answered before `deadline` or dropped.
fn ask(q: &[u8], upstream: SocketAddr, deadline: Instant) -> Option<Vec<u8>> {
    let left = || deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
    let mut u = TcpStream::connect_timeout(&upstream, left()?).ok()?;
    u.set_write_timeout(left()).ok()?;
    write_msg(&mut u, q).ok()?;
    u.set_read_timeout(left()).ok()?;
    let a = read_msg(&mut u)?;
    (a.len() >= 12 && a[..2] == q[..2]).then_some(a)
}

/// One length-prefixed DNS message; None at the end, a timeout or an error.
fn read_msg(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len).ok()?;
    let mut m = vec![0u8; usize::from(u16::from_be_bytes(len))];
    s.read_exact(&mut m).ok()?;
    Some(m)
}

fn write_msg(s: &mut TcpStream, m: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(m.len() + 2);
    b.extend_from_slice(&(m.len() as u16).to_be_bytes());
    b.extend_from_slice(m);
    s.write_all(&b)
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn relays_queries_verbatim_and_answers_to_the_asker() {
        let resolver = UdpSocket::bind("127.0.40.53:0").unwrap();
        resolver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut r, skipped) = Relay::new(&["127.0.40.1".parse().unwrap()], 0, resolver.local_addr().unwrap()).unwrap();
        assert!(skipped.is_empty());
        let at = r.socks[0].local_addr().unwrap();
        let client = UdpSocket::bind("127.0.40.2:0").unwrap();
        let stranger = UdpSocket::bind("127.0.40.9:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let query = [0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'c', b'o', b'm', 0, 0, 1, 0, 1];
        client.send_to(&query, at).unwrap();
        stranger.send_to(&[0x55, 0x55, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0], at).unwrap();
        let live = ["127.0.40.2".parse::<IpAddr>().unwrap()];
        thread::sleep(Duration::from_millis(50));
        r.pump(0, &live);
        let mut b = [0u8; 512];
        let (n, up) = resolver.recv_from(&mut b).unwrap();
        assert_eq!(&b[..n], &query, "verbatim");
        let mut answer = b[..n].to_vec();
        answer[2] |= 0x80;
        resolver.send_to(&answer, up).unwrap();
        resolver.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        assert!(resolver.recv_from(&mut b).is_err(), "a stranger's query is not relayed");
        thread::sleep(Duration::from_millis(50));
        r.pump(MS_TEST, &live);
        let (n, from) = client.recv_from(&mut b).unwrap();
        assert_eq!((&b[..n], from), (&answer[..], at));
        assert!(r.bonds[&"127.0.40.2".parse::<IpAddr>().unwrap()].inflight.is_empty());
        r.pump(MS_TEST, &[]);
        assert!(r.bonds.is_empty(), "a bond's state goes with it");
    }

    #[test]
    fn tcp_queries_are_relayed_with_the_same_bounds() {
        // The resolver over TCP: one answer per query, the ID kept.
        let resolver = TcpListener::bind("127.0.41.53:0").unwrap();
        let up = resolver.local_addr().unwrap();
        thread::spawn(move || {
            for mut s in resolver.incoming().flatten() {
                while let Some(mut q) = read_msg(&mut s) {
                    q[2] |= 0x80;
                    let _ = write_msg(&mut s, &q);
                }
            }
        });
        let (mut r, _) = Relay::new(&["127.0.41.1".parse().unwrap()], 0, up).unwrap();
        let at = r.tcp[0];
        let me: IpAddr = "127.0.41.2".parse().unwrap();
        r.pump(0, &[me]);
        let query = [0x43, 0x21, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'c', b'o', b'm', 0, 0, 1, 0, 1];
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket.bind(&SocketAddr::new(me, 0).into()).unwrap();
        socket.connect(&at.into()).unwrap();
        let mut c: TcpStream = socket.into();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        for _ in 0..2 {
            write_msg(&mut c, &query).unwrap();
            let a = read_msg(&mut c).expect("an answer");
            assert_eq!((&a[..2], a[2] & 0x80, &a[3..]), (&query[..2], 0x80, &query[3..]), "verbatim both ways");
        }
        // Every place taken: the query is dropped, unanswered.
        r.shared.udp.lock().unwrap().insert(me, DNS_INFLIGHT);
        write_msg(&mut c, &query).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        assert!(read_msg(&mut c).is_none(), "past DNS_INFLIGHT a query is dropped");
        r.shared.udp.lock().unwrap().clear();
        // A stranger is not served.
        let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        s.bind(&"127.0.41.9:0".parse::<SocketAddr>().unwrap().into()).unwrap();
        s.connect(&at.into()).unwrap();
        let mut s: TcpStream = s.into();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let _ = write_msg(&mut s, &query);
        assert!(read_msg(&mut s).is_none(), "a stranger's connection is closed");
        drop(r);
    }

    #[test]
    fn a_family_the_host_lacks_is_skipped() {
        // Inside `unshare -n`, ::1 exists only if IPv6 is up; a v6 address no interface has fails
        // with something other than EAFNOSUPPORT and is reported; IPv4 still serves.
        let (r, skipped) = Relay::new(
            &["127.0.42.1".parse().unwrap(), "2001:db8::1".parse().unwrap()],
            0,
            "127.0.0.53:53".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(r.socks.len(), 1, "IPv4 serves");
        assert!(skipped.iter().all(|(a, _)| a.is_ipv6()), "{skipped:?}");
        assert!(Relay::new(&["2001:db8::1".parse().unwrap()], 0, "127.0.0.53:53".parse().unwrap()).is_err());
    }

    const MS_TEST: Mono = 1_000_000;
}
