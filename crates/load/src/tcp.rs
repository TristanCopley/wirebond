//! TCP bulk throughput through an echo: one connection writes a pattern for a number of bytes or a
//! time while it reads the echo back, checking every byte.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crate::json::{Json, obj};
use crate::stats::{Meter, Proc, loadavg, round3};

#[derive(Clone, Debug)]
pub struct TcpConfig {
    pub to: SocketAddr,
    /// Bytes to send; 0: send for `duration`.
    pub bytes: u64,
    pub duration: Duration,
    /// Give up after this.
    pub timeout: Duration,
    pub procs: Vec<Proc>,
}

/// The pattern's byte at offset i: a period of 251 does not line up with any buffer size.
fn pattern(i: u64) -> u8 {
    (i % 251) as u8
}

/// The transfer: bytes echoed and checked, or what went wrong after how many.
fn transfer(c: &TcpConfig) -> (u64, Option<String>) {
    let mut s = match TcpStream::connect_timeout(&c.to, Duration::from_secs(10).min(c.timeout)) {
        Ok(s) => s,
        Err(e) => return (0, Some(format!("connect: {e}"))),
    };
    let _ = s.set_read_timeout(Some(c.timeout));
    let _ = s.set_write_timeout(Some(c.timeout));
    let mut w = match s.try_clone() {
        Ok(w) => w,
        Err(e) => return (0, Some(e.to_string())),
    };
    let (total, until) = (c.bytes, Instant::now() + c.duration);
    let writer = thread::spawn(move || -> io::Result<u64> {
        let mut b = vec![0u8; 64 << 10];
        let mut off = 0u64;
        while if total > 0 { off < total } else { Instant::now() < until } {
            let k = if total > 0 { (total - off).min(b.len() as u64) as usize } else { b.len() };
            for (j, x) in b[..k].iter_mut().enumerate() {
                *x = pattern(off + j as u64);
            }
            w.write_all(&b[..k])?;
            off += k as u64;
        }
        w.shutdown(Shutdown::Write)?;
        Ok(off)
    });
    let mut b = vec![0u8; 256 << 10];
    let mut off = 0u64;
    let mut err = None;
    loop {
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(k) => {
                if let Some(j) = (0..k).find(|&j| b[j] != pattern(off + j as u64)) {
                    err = Some(format!("byte {} came back changed", off + j as u64));
                    break;
                }
                off += k as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                err = Some(format!("after {off} bytes: {e}"));
                break;
            }
        }
    }
    if err.is_some() {
        let _ = s.shutdown(Shutdown::Both);
    }
    match writer.join() {
        Ok(Ok(sent)) if err.is_none() && sent != off => (off, Some(format!("sent {sent} bytes, {off} came back"))),
        Ok(Err(e)) if err.is_none() => (off, Some(format!("write: {e}"))),
        _ => (off, err),
    }
}

/// One bulk transfer: bytes echoed, seconds, throughput one way, and whether it worked.
pub fn tcp(c: &TcpConfig) -> Json {
    let meter = Meter::start(&c.procs);
    let t0 = Instant::now();
    let (bytes, err) = transfer(c);
    let secs = t0.elapsed().as_secs_f64().max(1e-9);
    let procs = meter.stop();
    let mut r = obj([
        ("to", c.to.to_string().into()),
        ("bytes", bytes.into()),
        ("secs", round3(secs).into()),
        ("mbps", round3(bytes as f64 * 8.0 / secs / 1e6).into()),
        ("ok", err.is_none().into()),
        ("procs", procs),
        ("load", loadavg().into()),
    ]);
    if let Some(e) = err {
        r.set("err", e);
    }
    r
}
