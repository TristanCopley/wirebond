//! What the daemon's test files share: a fake TUN device and their loopback addresses. Each file
//! keeps its own fake host, whose calls its assertions read.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use super::plat::TunDev;

/// A fake TUN device: what the test writes for the daemon to read, and what the daemon wrote.
#[derive(Clone, Default)]
pub struct FakeTun(Arc<Mutex<Queues>>);

/// What the daemon will read, and what it wrote.
type Queues = (VecDeque<Vec<u8>>, Vec<Vec<u8>>);

impl FakeTun {
    pub fn inject(&self, p: Vec<u8>) {
        self.0.lock().unwrap().0.push_back(p);
    }

    pub fn written(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().1.clone()
    }
}

impl TunDev for FakeTun {
    fn name(&self) -> &str {
        "fake0"
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let p = self.0.lock().unwrap().0.pop_front().ok_or(io::ErrorKind::WouldBlock)?;
        buf[..p.len()].copy_from_slice(&p);
        Ok(p.len())
    }

    fn write(&self, packet: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().1.push(packet.to_vec());
        Ok(())
    }
}

/// 127.`net`.0.`host`.
pub fn ip(net: u8, host: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, net, 0, host))
}
