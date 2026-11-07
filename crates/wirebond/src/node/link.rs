//! Links, lane sockets and STUN (SPEC 2.0 §8, §9): kept from 1.x, which ran them. The daemon lists
//! links every LINK_SCAN (`host::links`), opens one pinned socket per link (`host::listen`), learns
//! each link's public mapping with STUN, and groups links into bottleneck groups. Owner: node
//! daemon.
//!
//! STUN ([`Stun`], one per link socket) asks the configured servers on every new link, on a wake,
//! and every STUN_EVERY while data flows; a link offers its mappings as candidates (§9.3). Links
//! that share an interface, a public IPv4 from STUN or a default gateway share a bottleneck group
//! ([`groups`]).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use socket2::{Domain, Socket, Type};
use wirebond_core::{Mono, STUN_EVERY, STUN_WAIT};

/// RFC 8489's magic cookie, at bytes 4–7 of every STUN message.
pub const STUN_COOKIE: u32 = 0x2112_A442;

/// One link's STUN (§9.2): the requests in flight, the mappings learned, and when to ask next.
#[derive(Debug, Default)]
pub struct Stun {
    /// Requests in flight: transaction id, server, sent at.
    pending: Vec<([u8; 12], SocketAddr, Mono)>,
    /// The mapping each answering server reported: (server, mapping).
    pub mapped: Vec<(SocketAddr, SocketAddr)>,
    /// When the next round is due; 0: at once (a new link).
    next: Mono,
    /// The round in flight: its start, and whether a server answered it.
    round: Option<(Mono, bool)>,
    /// The wait before asking again while no server answers.
    backoff: Mono,
    /// Ask at once, data or not: a new link, or a wake.
    asap: bool,
}

impl Stun {
    /// Whether a round is due at `now`: at once on a new link (or after [`Stun::again`]), then every
    /// STUN_EVERY while data flows (`flowing`); while no server has ever answered, after a backoff
    /// that doubles up to STUN_EVERY.
    pub fn due(&mut self, now: Mono, flowing: bool) -> bool {
        if let Some((at, answered)) = self.round
            && now - at >= STUN_WAIT
        {
            (self.round, self.pending) = (None, Vec::new());
            if !answered {
                self.backoff = (self.backoff * 2).clamp(2 * STUN_WAIT, STUN_EVERY);
                self.next = at + self.backoff;
            }
        }
        self.round.is_none() && now >= self.next && (flowing || self.asap || self.mapped.is_empty())
    }

    /// Starts a round at `now`: one Binding request per server, each with a fresh transaction id
    /// from `rand`. Returns what to send where.
    pub fn round(
        &mut self,
        now: Mono,
        servers: &[SocketAddr],
        mut rand: impl FnMut() -> [u8; 12],
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        (self.round, self.next, self.asap) = (Some((now, false)), now + STUN_EVERY, false);
        servers
            .iter()
            .map(|&to| {
                let t = rand();
                self.pending.push((t, to, now));
                (to, stun_request(&t))
            })
            .collect()
    }

    /// A datagram `b` from `from` at `now`: None unless it answers a request in flight, which only
    /// a server queried within STUN_WAIT can; else whether the link's mappings changed.
    pub fn answer(&mut self, now: Mono, from: SocketAddr, b: &[u8]) -> Option<bool> {
        let (t, mapped) = stun_parse(b)?;
        let i = self.pending.iter().position(|p| p.0 == t && p.1 == from && now - p.2 < STUN_WAIT)?;
        self.pending.swap_remove(i);
        let Some(m) = mapped.map(unmap) else { return Some(false) };
        if let Some((_, answered)) = self.round.as_mut() {
            *answered = true;
        }
        self.backoff = 0;
        Some(match self.mapped.iter_mut().find(|(s, _)| *s == from) {
            Some(e) => std::mem::replace(&mut e.1, m) != m,
            None => {
                self.mapped.push((from, m));
                true
            }
        })
    }

    /// Asks again at once, keeping the mappings until the answers come: on a wake (§11.4).
    pub fn again(&mut self) {
        (self.next, self.round, self.pending, self.backoff, self.asap) = (0, None, Vec::new(), 0, true);
    }

    /// The mappings learned, each once.
    pub fn mappings(&self) -> Vec<SocketAddr> {
        let mut v: Vec<SocketAddr> = Vec::new();
        for &(_, m) in &self.mapped {
            if !v.contains(&m) {
                v.push(m);
            }
        }
        v
    }

    /// The public IPv4 addresses STUN reported.
    pub fn public4(&self) -> Vec<Ipv4Addr> {
        self.mapped.iter().filter_map(|(_, m)| if let IpAddr::V4(a) = m.ip() { Some(a) } else { None }).collect()
    }
}

/// One link as [`groups`] sees it.
pub struct GroupKey<'a> {
    pub id: u8,
    /// The interface: its IPv4 and IPv6 links share it.
    pub name: &'a str,
    pub public4: Vec<Ipv4Addr>,
    pub gateway: Option<IpAddr>,
}

/// Bottleneck groups (§8): each link is its own group unless it shares its interface, a public
/// IPv4 from STUN or a default gateway with another, transitively; a group is numbered by its
/// lowest link id. Returns each link's group, in `links`' order.
pub fn groups(links: &[GroupKey<'_>]) -> Vec<u8> {
    let near = |a: &GroupKey<'_>, b: &GroupKey<'_>| {
        a.name == b.name
            || a.public4.iter().any(|p| b.public4.contains(p))
            || a.gateway.is_some() && a.gateway == b.gateway
    };
    let mut g: Vec<u8> = links.iter().map(|l| l.id).collect();
    // At most MAX_LINKS links a family: relaxing to the fixpoint is cheap.
    let mut moved = true;
    while moved {
        moved = false;
        for i in 0..links.len() {
            for j in 0..links.len() {
                if g[j] < g[i] && near(&links[i], &links[j]) {
                    (g[i], moved) = (g[j], true);
                }
            }
        }
    }
    g
}
/// A peer's link label at most, in characters: status shows it as a lane's name.
pub const LABEL_MAX: usize = 32;

/// The captive-portal check (§17): host, port and path.
pub const CAPTIVE_URL: (&str, u16, &str) = ("captive.apple.com", 80, "/hotspot-detect.html");
/// How long the check may take.
pub const CAPTIVE_WAIT: Duration = Duration::from_secs(5);

/// The captive-portal check over the link `name` whose address is `from` (§17): GET `path` from
/// `to`. Some(true) when anything but `Success` answers (a portal), Some(false) for `Success`,
/// None when nothing answered. On Linux the socket is pinned to the link and marked as the lane
/// sockets are (0x7849), so it bypasses capture.
pub fn captive(from: IpAddr, name: &str, to: SocketAddr, host: &str, path: &str) -> Option<bool> {
    let s = Socket::new(Domain::for_address(to), Type::STREAM, None).ok()?;
    #[cfg(target_os = "linux")]
    {
        let _ = s.set_mark(0x7849);
        let _ = s.bind_device(Some(name.as_bytes()));
    }
    let _ = name;
    s.bind(&SocketAddr::new(from, 0).into()).ok()?;
    s.connect_timeout(&to.into(), CAPTIVE_WAIT).ok()?;
    let mut s: TcpStream = s.into();
    s.set_read_timeout(Some(CAPTIVE_WAIT)).ok()?;
    s.set_write_timeout(Some(CAPTIVE_WAIT)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: CaptiveNetworkSupport\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut b = Vec::new();
    let _ = s.take(64 << 10).read_to_end(&mut b);
    if b.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&b);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let ok = head.split_whitespace().nth(1) == Some("200") && body.contains("Success");
    Some(!ok)
}

/// A Binding request with transaction id t: type 0x0001, no attributes, the cookie, then t.
pub fn stun_request(t: &[u8; 12]) -> Vec<u8> {
    [&[0, 1, 0, 0][..], &STUN_COOKIE.to_be_bytes(), t].concat()
}

/// A STUN message's transaction id and, from a Binding success response, its
/// XOR-MAPPED-ADDRESS (IPv4 or IPv6). None when the magic cookie is not at bytes 4–7.
pub fn stun_parse(b: &[u8]) -> Option<([u8; 12], Option<SocketAddr>)> {
    if b.len() < 20 || b[4..8] != STUN_COOKIE.to_be_bytes() {
        return None;
    }
    let t: [u8; 12] = b[8..20].try_into().ok()?;
    let key = &b[4..20]; // the cookie, then the transaction id: XORed into the address
    let mut mapped = None;
    let mut rest = &b[20..];
    while b[..2] == [1, 1] && rest.len() >= 4 {
        let (typ, n) = (u16::from_be_bytes([rest[0], rest[1]]), usize::from(u16::from_be_bytes([rest[2], rest[3]])));
        let Some(v) = rest.get(4..4 + n) else { break };
        if typ == 0x0020 && (n == 8 && v[1] == 1 || n == 20 && v[1] == 2) {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ (STUN_COOKIE >> 16) as u16;
            let mut ip = [0u8; 16];
            ip.iter_mut().zip(&v[4..]).zip(key).for_each(|((o, x), k)| *o = x ^ k);
            let ip = if n == 8 { IpAddr::from([ip[0], ip[1], ip[2], ip[3]]) } else { IpAddr::from(ip) };
            mapped = Some(SocketAddr::new(ip, port));
        }
        rest = &rest[(4 + ((n + 3) & !3)).min(rest.len())..];
    }
    Some((t, mapped))
}

/// a, its IPv4-mapped address unmapped.
pub fn unmap(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

/// s without control and format characters (C0, DEL, C1, bidirectional and zero-width marks, line
/// and paragraph separators, tags), at most `max` characters: a peer's string as status may show
/// it on a terminal.
pub fn clean(s: &str, max: usize) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}')
            || matches!(c, '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0000}'..='\u{e007f}')
    };
    s.chars().filter(|&c| !hidden(c)).take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Binding success response for transaction `t` mapping `a`.
    fn stun_response(t: &[u8; 12], a: SocketAddr) -> Vec<u8> {
        let mut key = STUN_COOKIE.to_be_bytes().to_vec();
        key.extend_from_slice(t);
        let (family, ip) = match a.ip() {
            IpAddr::V4(v) => (1u8, v.octets().to_vec()),
            IpAddr::V6(v) => (2u8, v.octets().to_vec()),
        };
        let mut v = vec![0, family];
        v.extend_from_slice(&(a.port() ^ (STUN_COOKIE >> 16) as u16).to_be_bytes());
        v.extend(ip.iter().zip(&key).map(|(x, k)| x ^ k));
        let mut b = vec![1, 1];
        b.extend_from_slice(&((4 + v.len()) as u16).to_be_bytes());
        b.extend_from_slice(&key);
        b.extend_from_slice(&[0, 0x20]);
        b.extend_from_slice(&(v.len() as u16).to_be_bytes());
        b.extend_from_slice(&v);
        b
    }

    #[test]
    fn stun_parse_reads_the_mapping() {
        let t = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        for want in ["203.0.113.9:61022", "[2607:fb90::1]:7849"].map(|s| s.parse::<SocketAddr>().unwrap()) {
            assert_eq!(stun_parse(&stun_response(&t, want)), Some((t, Some(want))));
        }
        let req = stun_request(&t);
        assert_eq!(stun_parse(&req), Some((t, None)), "a request has the cookie and no mapping");
        let good = stun_response(&t, "198.51.100.1:1".parse().unwrap());
        for i in 0..good.len() {
            let _ = stun_parse(&good[..i]); // truncations never panic
        }
        let mut bad = good.clone();
        bad[5] ^= 1;
        assert_eq!(stun_parse(&bad), None, "a wrong cookie is not STUN");
    }

    #[test]
    fn stun_takes_answers_from_a_server_queried_within_stun_wait() {
        use wirebond_core::SEC;
        let (srv, other): (SocketAddr, SocketAddr) =
            ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        let m: SocketAddr = "203.0.113.9:61022".parse().unwrap();
        let mut s = Stun::default();
        let mut n = 0u8;
        let mut rand = || {
            n += 1;
            [n; 12]
        };
        assert!(s.due(0, false), "a new link asks at once");
        let t = stun_parse(&s.round(0, &[srv], &mut rand)[0].1).unwrap().0;
        assert_eq!(s.answer(SEC, other, &stun_response(&t, m)), None, "not the server queried");
        assert_eq!(s.answer(STUN_WAIT, srv, &stun_response(&t, m)), None, "after STUN_WAIT");
        assert!(!s.due(STUN_WAIT, false), "no answer: a backoff first");
        assert!(s.due(2 * STUN_WAIT, false));
        let t = stun_parse(&s.round(2 * STUN_WAIT, &[srv], &mut rand)[0].1).unwrap().0;
        assert_eq!(s.answer(2 * STUN_WAIT + SEC, srv, &stun_response(&t, m)), Some(true));
        assert_eq!(s.answer(2 * STUN_WAIT + SEC, srv, &stun_response(&t, m)), None, "each request once");
        assert_eq!((s.mappings(), s.public4()), (vec![m], vec![Ipv4Addr::new(203, 0, 113, 9)]));
        let later = 2 * STUN_WAIT + STUN_EVERY;
        assert!(!s.due(later, false), "a refresh only while data flows");
        assert!(s.due(later, true));
        s.again();
        assert!(s.due(later, false), "a wake asks at once");
        assert_eq!(s.mappings(), vec![m], "the mapping stays until the answers come");
    }

    #[test]
    fn groups_join_by_interface_public_address_and_gateway() {
        let g = |id, name, public4: &[[u8; 4]], gw: Option<&str>| GroupKey {
            id,
            name,
            public4: public4.iter().map(|&a| Ipv4Addr::from(a)).collect(),
            gateway: gw.map(|g| g.parse().unwrap()),
        };
        let links = [
            g(1, "en0", &[[203, 0, 113, 7]], Some("192.168.1.1")),
            g(2, "en0", &[], None), // en0's IPv6 link
            g(3, "en5", &[[198, 51, 100, 1]], Some("192.168.1.1")),
            g(4, "en7", &[[198, 51, 100, 1]], Some("172.20.10.1")),
            g(5, "en8", &[[192, 0, 2, 9]], Some("10.0.0.1")),
        ];
        assert_eq!(groups(&links), vec![1, 1, 1, 1, 5], "transitively: en7 shares en5's public address");
        assert_eq!(groups(&links[3..]), vec![4, 5]);
    }

    /// Serves one answer per connection on loopback; returns its address.
    fn http(answer: &'static str) -> SocketAddr {
        let ln = std::net::TcpListener::bind("127.0.41.80:0").unwrap();
        let at = ln.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in ln.incoming().flatten() {
                let mut b = [0u8; 1024];
                let _ = s.read(&mut b);
                let _ = s.write_all(answer.as_bytes());
            }
        });
        at
    }

    #[test]
    fn captive_tells_a_portal_from_success() {
        let from: IpAddr = "127.0.41.1".parse().unwrap();
        let ok = http("HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n\r\n<HTML><BODY>Success</BODY></HTML>");
        assert_eq!(captive(from, "lo", ok, "captive.apple.com", "/hotspot-detect.html"), Some(false));
        let portal = http("HTTP/1.0 302 Found\r\nLocation: http://login.hotel/\r\n\r\n");
        assert_eq!(captive(from, "lo", portal, "captive.apple.com", "/"), Some(true));
        let page = http("HTTP/1.0 200 OK\r\n\r\n<html>Log in to the hotel Wi-Fi</html>");
        assert_eq!(captive(from, "lo", page, "captive.apple.com", "/"), Some(true));
        let closed = std::net::TcpListener::bind("127.0.41.81:0").unwrap().local_addr().unwrap();
        assert_eq!(captive(from, "lo", closed, "captive.apple.com", "/"), None, "no answer is not a portal");
    }

    #[test]
    fn clean_drops_hidden_characters() {
        assert_eq!(clean("Wi\u{202e}-Fi\n", LABEL_MAX), "Wi-Fi");
        assert_eq!(clean(&"x".repeat(40), LABEL_MAX).len(), LABEL_MAX);
        assert_eq!(unmap("[::ffff:192.0.2.1]:7".parse().unwrap()), "192.0.2.1:7".parse().unwrap());
    }
}
