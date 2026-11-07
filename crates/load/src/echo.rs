//! The echo: the service the harness's senders talk to, in the "internet" server's namespace and on
//! the anchor. UDP datagrams go back to their sender as they came; TCP connections get back every
//! byte until the peer closes its side. It counts what arrives by protocol, port and source
//! address, which shows whether traffic was NATed at the anchor (the anchor's uplink address),
//! delivered locally (the client's bond address) or forged (any other), as SPEC §6 and A8/A9 ask.
//!
//! Counters: `{"udp:47998:10.77.0.2": {"pkts": N, "bytes": N}, "tcp:47989:10.80.0.1": {"conns":
//! N, "bytes": N}}`, IPv4-mapped IPv6 sources written as IPv4.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::json::{Json, obj};

type Counts = Arc<Mutex<BTreeMap<String, (u64, u64)>>>;

/// A running echo: its sockets serve on their own threads for as long as the process lives.
pub struct Echo {
    pub udp: Vec<SocketAddr>,
    pub tcp: Vec<SocketAddr>,
    counts: Counts,
}

fn add(counts: &Counts, key: String, n: u64, bytes: u64) {
    let mut m = counts.lock().unwrap_or_else(|e| e.into_inner());
    let c = m.entry(key).or_default();
    c.0 += n;
    c.1 += bytes;
}

fn key(proto: &str, port: u16, from: IpAddr) -> String {
    format!("{proto}:{port}:{}", from.to_canonical())
}

/// Binds `addr:port`: with the unspecified address, IPv6's (dual stack, as Linux binds it by
/// default) and else IPv4's.
fn bind<T>(addr: Option<IpAddr>, port: u16, f: impl Fn(SocketAddr) -> io::Result<T>) -> io::Result<T> {
    match addr {
        Some(a) => f(SocketAddr::new(a, port)),
        None => f(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port))
            .or_else(|_| f(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))),
    }
}

impl Echo {
    /// Starts the echo on each port, on `addr` or on every address. Port 0 picks one; `udp` and
    /// `tcp` hold the addresses bound.
    pub fn start(addr: Option<IpAddr>, udp: &[u16], tcp: &[u16]) -> io::Result<Echo> {
        let counts: Counts = Arc::default();
        let mut e = Echo { udp: vec![], tcp: vec![], counts: counts.clone() };
        for &p in udp {
            let s = bind(addr, p, UdpSocket::bind)?;
            let local = s.local_addr()?;
            e.udp.push(local);
            let c = counts.clone();
            thread::spawn(move || udp_echo(&s, local.port(), &c));
        }
        for &p in tcp {
            let l = bind(addr, p, TcpListener::bind)?;
            let local = l.local_addr()?;
            e.tcp.push(local);
            let c = counts.clone();
            thread::spawn(move || {
                for conn in l.incoming() {
                    match conn {
                        Ok(s) => {
                            let c = c.clone();
                            thread::spawn(move || tcp_echo(s, local.port(), &c));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
        }
        Ok(e)
    }

    /// The counters so far.
    pub fn stats(&self) -> Json {
        let m = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        Json::Obj(
            m.iter()
                .map(|(k, &(n, bytes))| {
                    let what = if k.starts_with("tcp:") { "conns" } else { "pkts" };
                    (k.clone(), obj([(what, n.into()), ("bytes", bytes.into())]))
                })
                .collect(),
        )
    }
}

fn udp_echo(s: &UdpSocket, port: u16, counts: &Counts) {
    let mut b = vec![0u8; 65536];
    loop {
        match s.recv_from(&mut b) {
            Ok((n, from)) => {
                add(counts, key("udp", port, from.ip()), 1, n as u64);
                let _ = s.send_to(&b[..n], from);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // ICMP errors from an earlier reply surface here: carry on.
            Err(_) => {}
        }
    }
}

fn tcp_echo(mut s: TcpStream, port: u16, counts: &Counts) {
    let from = s.peer_addr().map(|a| a.ip()).unwrap_or(Ipv4Addr::UNSPECIFIED.into());
    let k = key("tcp", port, from);
    add(counts, k.clone(), 1, 0);
    let _ = s.set_nodelay(true);
    let mut b = vec![0u8; 256 << 10];
    loop {
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(n) => {
                add(counts, k.clone(), 0, n as u64);
                if s.write_all(&b[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
    let _ = s.shutdown(Shutdown::Write);
}

/// Writes the counters to path every 200 ms (to a temporary file, then renamed), forever.
pub fn serve(e: &Echo, stats: Option<&std::path::Path>) -> ! {
    loop {
        if let Some(p) = stats {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, format!("{}\n", e.stats())).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}
