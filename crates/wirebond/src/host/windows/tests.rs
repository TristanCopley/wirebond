//! windows.rs's tests. They run only on Windows (`cargo test -p wirebond` there), and leave the
//! service, the firewall and %ProgramData% alone: what touches those is checked by hand
//! (notes/rust.md, "host: Windows").

use super::*;
use crate::host::net::{Capture, Routes, TunConfig};
use socket2::{Domain, Type};
use std::fs;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Instant;

/// A fresh, empty directory under %TEMP%.
fn temp(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("wirebond-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn loopback() -> Iface {
    let lo = IpAddr::from(Ipv4Addr::LOCALHOST);
    let ifs = ifaces().unwrap();
    ifs.into_iter().find(|i| i.addrs.contains(&lo)).expect("the loopback interface is listed")
}

#[test]
fn lists_the_loopback_interface() {
    let lo = loopback();
    assert!(lo.up && lo.index != 0 && !lo.wifi && !lo.name.is_empty(), "{}", lo.name);
}

#[test]
fn pins_sizes_and_survives_port_unreachable() {
    let lo = loopback();
    let s = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    let (rcv, snd) = sockopts(&s, false, lo.index, &lo.name).unwrap();
    assert!(rcv >= SOCK_BUF && snd >= SOCK_BUF, "granted {rcv} and {snd}");
    s.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into()).unwrap();
    // A closed port answers with ICMP port unreachable. With SIO_UDP_CONNRESET on, the next read
    // fails with WSAECONNRESET; with it off, the read only times out.
    let closed = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    closed.bind(&SocketAddr::from((Ipv4Addr::LOCALHOST, 0)).into()).unwrap();
    let to = closed.local_addr().unwrap();
    std::mem::drop(closed);
    s.send_to(b"x", &to).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let mut buf = [MaybeUninit::uninit(); 16];
    let e = s.recv_from(&mut buf).unwrap_err();
    assert!(matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock), "{e}");
}

#[test]
fn boot_counts_and_paths_are_fixed() {
    let a = boot();
    thread::sleep(Duration::from_millis(100));
    assert!(a > 0 && boot() - a >= 50_000_000); // GetTickCount64 ticks every 10 to 16 ms
    let (exe, dir) = paths(Scope::System);
    assert!(exe.ends_with(r"wirebond\wirebond.exe") && dir.ends_with("wirebond"));
    assert!(exe.is_absolute() && dir.is_absolute());
}

#[test]
fn local_sockets_round_trip_and_status_opens_up() {
    let dir = temp("sock");
    fs::create_dir(&dir).unwrap();
    let path = dir.join(super::super::STATUS_SOCK);
    let l = super::super::local(&path, true).unwrap();
    public(&path).unwrap();
    let t = thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut b = [0; 4];
        c.read_exact(&mut b).unwrap();
        c.write_all(&b).unwrap();
    });
    let mut c = super::super::local(&path, false).unwrap();
    c.write_all(b"ping").unwrap();
    let mut b = [0; 4];
    c.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"ping");
    t.join().unwrap();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn state_directory_is_made_and_checked() {
    if !admin() {
        return; // the owner, administrators, needs an elevated token
    }
    let dir = temp("state");
    mk_state(&dir).unwrap(); // made
    mk_state(&dir).unwrap(); // its own, accepted
    fs::remove_dir_all(&dir).unwrap();
    fs::create_dir(&dir).unwrap(); // an ordinary one: its DACL is not protected
    assert!(mk_state(&dir).is_err());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn run_service_makes_waits_accurate_to_a_millisecond() {
    // Review: windows timer. node's loop waits in Poll::wait, whose WSAPoll timeout ends on a timer
    // tick, often 15.6 ms apart unless the process asks for 1 ms timers, as run_service does. A
    // child of this test binary, where no other test has asked and run_service may start the
    // service dispatcher (once a process; run_service_never_stops does in this one), times 50 waits
    // of 1 ms before run_service, inside it and after it. It prints them to the stderr it shares
    // with this process, so CI's log shows them even when the test passes.
    let name = "host::os::tests::run_service_makes_waits_accurate_to_a_millisecond";
    if env::var_os("WIREBOND_TEST_TIMER").is_none() {
        let child = Command::new(env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("WIREBOND_TEST_TIMER", "1")
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&child.stdout);
        assert!(child.status.success() && out.contains("1 passed"), "the child ran the test: {out}");
        return;
    }
    let before = overshoots();
    let mut inside = Vec::new();
    run_service(|_| {
        inside = overshoots();
        Ok(())
    })
    .unwrap();
    let after = overshoots();
    let row = |when: &str, v: &[f64]| {
        format!(
            "\n  {when:18} min {:5.2}  median {:5.2}  90th percentile {:5.2}  max {:5.2}",
            v[0], v[25], v[45], v[49]
        )
    };
    let (b, i, a) = (row("before run_service", &before), row("inside it", &inside), row("after it returns", &after));
    eprintln!("{name}: how far each of 50 waits of 1 ms overshoots it, in ms:{b}{i}{a}");
    assert!(inside[25] < 3.0, "inside run_service the median wait of 1 ms overshoots it by {:.2} ms", inside[25]);
}

/// How far each of 50 waits of 1 ms in node's Poll, on a Waker alone, overshoots it, in ms, sorted.
fn overshoots() -> Vec<f64> {
    let (mut p, w) = (super::super::Poll::default(), super::super::Waker::new().unwrap());
    let mut v: Vec<f64> = (0..50)
        .map(|_| {
            p.clear();
            p.add_waker(&w);
            let at = Instant::now();
            p.wait(Duration::from_millis(1)).unwrap();
            at.elapsed().as_secs_f64() * 1e3 - 1.0
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v
}

#[test]
fn deletes_wait_for_a_detached_cmd() {
    let dir = temp("later");
    fs::create_dir(&dir).unwrap();
    let (download, exe) = (dir.join("download.exe"), dir.join("bin").join("wirebond.exe"));
    fs::write(&download, b"x").unwrap();
    fs::create_dir(exe.parent().unwrap()).unwrap();
    fs::write(&exe, b"x").unwrap();
    super::drop(&download);
    assert!(remove_exe(&exe).unwrap());
    assert!(download.exists() && exe.exists()); // about 2 s from now, not yet
    thread::sleep(Duration::from_secs(5));
    assert!(!download.exists() && !exe.parent().unwrap().exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn event_loop_wakes_on_a_datagram_and_on_time() {
    let ev = EventLoop::new().unwrap();
    let u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    ev.add(&u).unwrap();
    let at = Instant::now();
    ev.wait(None, Duration::from_millis(20)).unwrap();
    assert!(at.elapsed() >= Duration::from_millis(19), "woke after {:?}", at.elapsed());
    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    tx.send_to(b"x", u.local_addr().unwrap()).unwrap();
    let at = Instant::now();
    ev.wait(None, Duration::from_secs(5)).unwrap();
    assert!(at.elapsed() < Duration::from_secs(2), "the datagram did not wake it");
    let mut b = [0; 8];
    assert_eq!(u.recv_from(&mut b).unwrap().0, 1);
    assert_eq!(u.recv_from(&mut b).unwrap_err().kind(), io::ErrorKind::WouldBlock); // WSAEventSelect: nonblocking
}

#[test]
fn opens_the_wintun_adapter_as_administrator() {
    let c = TunConfig {
        name: "wirebond-test".into(),
        mtu: 1400,
        addr4: Some((Ipv4Addr::new(10, 77, 0, 2), 24)),
        addr6: None,
    };
    let t = match Tun::open(&c) {
        Ok(t) => t,
        Err(e) if !admin() || e.to_string().contains("no wintun.dll embedded") => return, // CI fetches it
        Err(e) => panic!("{e}"),
    };
    assert_eq!(t.name(), "wirebond-test");
    assert!(!t.read_event().is_null());
    let mut b = [0u8; 2048];
    match t.read(&mut b) {
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
        Ok(n) => assert!(n > 0),
    }
    let net = Some((Ipv4Addr::new(10, 77, 0, 0), 24));
    let cap = Capture {
        tun: c.name.clone(),
        routes: Routes::All,
        bond4: net,
        bond6: None,
        exclude: Vec::new(),
        links: Vec::new(),
        dns: None,
    };
    capture_install(&cap).unwrap();
    capture_install(&cap).unwrap(); // idempotent
    capture_remove(&cap).unwrap();
    capture_remove(&cap).unwrap();
}
