//! The support role on a fake host: sharing follows the config, a refusal prints the manual
//! steps, the registration names the client's link, and the cable's OS counters are carried
//! bytes. Run inside `unshare -n` with `lo` up.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::super::plat::{Platform, TunDev};
use super::super::{Daemon, Request};
use super::*;
use crate::host::Config;

/// A fake host: one link, one mesh address, a sharing that works unless `refuse`, and cable
/// counters that grow by 1000 bytes each way per read.
struct Fake {
    name: String,
    link: host::Link,
    mesh: host::Link,
    refuse: bool,
    calls: Arc<Mutex<Vec<String>>>,
    bytes: u64,
}

impl Platform for Fake {
    fn open_tun(&mut self, _: &host::TunConfig) -> io::Result<Box<dyn TunDev>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn capture_install(&mut self, _: &host::Capture) -> io::Result<()> {
        Ok(())
    }
    fn capture_remove(&mut self, _: &host::Capture) -> io::Result<()> {
        Ok(())
    }
    fn egress_enable(&mut self, _: &host::Egress) -> io::Result<host::EgressOn> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn egress_disable(&mut self, _: &host::Egress) -> io::Result<()> {
        Ok(())
    }
    fn upstream_resolver(&mut self) -> io::Result<SocketAddr> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn links(&mut self, _: &[(IpAddr, u8)]) -> io::Result<(Vec<host::Link>, Vec<host::Link>)> {
        Ok((vec![self.link.clone()], vec![self.mesh.clone()]))
    }
    fn bind(&mut self, link: &host::Link, mesh: bool) -> io::Result<UdpSocket> {
        UdpSocket::bind((link.addr, if mesh { wirebond_core::PORT } else { 0 }))
    }
    fn hostname(&self) -> String {
        self.name.clone()
    }
    fn support_share(&mut self, s: &host::Share) -> io::Result<()> {
        self.calls.lock().unwrap().push(format!("share {} {}", s.uplink, s.cable));
        if self.refuse { Err(io::Error::other("refused")) } else { Ok(()) }
    }
    fn support_unshare(&mut self, s: &host::Share) -> io::Result<()> {
        self.calls.lock().unwrap().push(format!("unshare {} {}", s.uplink, s.cable));
        Ok(())
    }
    fn if_bytes(&mut self, l: &host::Link) -> io::Result<(u64, u64)> {
        assert_eq!(l.name, "cable0");
        self.bytes += 1000;
        Ok((self.bytes, self.bytes))
    }
}

fn ip(net: u8, host: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, net, 0, host))
}

/// A node named `name` whose one link `link_name` is at `link`, with mesh address `mesh`.
fn node(
    name: &str,
    link_name: &str,
    link: IpAddr,
    mesh: IpAddr,
    refuse: bool,
    cfg: Config,
) -> (Daemon, Arc<Mutex<Vec<String>>>) {
    let l = |name: &str, addr| host::Link { index: 1, name: name.into(), addr, label: name.into(), metered: false };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        name: name.into(),
        link: l(link_name, link),
        mesh: l("mesh0", mesh),
        refuse,
        calls: calls.clone(),
        bytes: 0,
    };
    let mut d = Daemon::new("2.0.0", None, cfg, Box::new(fake)).unwrap();
    (d.loopback, d.captive_url) = (true, None);
    (d, calls)
}

fn ask(d: &mut Daemon, req: Request) -> api::Reply {
    let (tx, rx) = std::sync::mpsc::channel();
    d.call(req, tx);
    rx.try_recv().unwrap()
}

fn on(cable: &str, for_client: Option<String>) -> Request {
    Request::SupportOn { uplink: "wlan0".into(), cable: cable.into(), for_client }
}

#[test]
fn support_on_shares_and_off_unshares() {
    let (mut d, calls) = node("pc", "cable0", ip(60, 1), ip(60, 10), false, Config::default());
    let r = ask(&mut d, on("cable0", None));
    assert!(r.ok && r.changed, "{r:?}");
    assert_eq!(*calls.lock().unwrap(), ["share wlan0 cable0"]);
    let r = ask(&mut d, Request::SupportOff);
    assert!(r.ok && r.changed, "{r:?}");
    assert_eq!(*calls.lock().unwrap(), ["share wlan0 cable0", "unshare wlan0 cable0"]);
    d.reconcile();
    assert_eq!(calls.lock().unwrap().len(), 2, "nothing more to undo");
}

#[test]
fn a_refused_share_prints_the_manual_steps() {
    let (mut d, calls) = node("pc", "cable0", ip(61, 1), ip(61, 10), true, Config::default());
    let r = ask(&mut d, on("cable0", None));
    assert!(!r.ok, "{r:?}");
    let msg = r.error.map(|e| e.msg).unwrap_or_default();
    assert!(msg.contains("share it by hand") && msg.contains("cable0"), "{msg}");
    d.reconcile();
    assert_eq!(calls.lock().unwrap().len(), 1, "not tried again until the config changes");
}

#[test]
fn the_cable_counters_are_the_bytes_carried() {
    let (mut d, _) = node("pc", "cable0", ip(62, 1), ip(62, 10), false, Config::default());
    d.scan();
    assert!(ask(&mut d, on("cable0", None)).ok);
    d.support.next_count = 0;
    d.support_tick();
    let st = d.status(false);
    assert_eq!((st.support.map(|s| s.carried_bytes), st.contribution.rx_bytes), (Some(2000), 1000));
}

#[test]
fn a_registered_support_names_the_clients_link() {
    let (mut s, _) = node("pc", "cable0", ip(63, 1), ip(63, 10), false, Config::default());
    let (mut c, _) = node("mac", "eth1", ip(63, 2), ip(63, 20), false, Config::default());
    let (mut ps, mut pc) = (host::Poll::default(), host::Poll::default());
    // The client's first step opens its control listener: the support's first dial must find it,
    // since a refused one is retried only after REDIAL (10 s), as long as this test waits. macOS's
    // threads lost that race about one run in two.
    c.step(&mut pc, Duration::from_millis(5)).unwrap();
    assert!(ask(&mut s, on("cable0", Some(ip(63, 20).to_string()))).ok);
    let t = Instant::now();
    loop {
        s.step(&mut ps, Duration::from_millis(5)).unwrap();
        c.step(&mut pc, Duration::from_millis(5)).unwrap();
        let st = c.status(false);
        if st.links.first().and_then(|l| l.support.clone()).as_deref() == Some("pc") {
            assert_eq!(c.offer().links[0].support.as_deref(), Some("pc"), "the anchor hears the name too");
            break;
        }
        let support = || s.status(false).events;
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no registration: client {:#?}, support {:#?}",
            st.events,
            support()
        );
    }
}
