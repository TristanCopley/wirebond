//! What macOS and Linux share: the interface listing, socket options, the sleep-inclusive clock,
//! root, the state directory and its guard, socket permissions, the daemon's entry, and files and
//! commands for register and unregister.

use super::{Iface, Scope};
use libc::c_int;
use socket2::{Domain, SockRef, Socket, Type};
use std::ffi::CStr;
use std::fs::{self, Permissions};
use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::unix::net::UnixDatagram;
use std::os::unix::prelude::*;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use std::{io, ptr};

/// The interfaces as getifaddrs lists them, Wi-Fi as the OS's `wifi` tells it.
pub(super) fn ifaces() -> io::Result<Vec<Iface>> {
    let mut head = ptr::null_mut();
    // SAFETY: getifaddrs points head at a list that freeifaddrs releases below.
    cvt(unsafe { libc::getifaddrs(&mut head) })?;
    let mut ifs: Vec<Iface> = Vec::new();
    let mut p = head;
    // SAFETY: p is null or a node of that list.
    while let Some(ifa) = unsafe { p.as_ref() } {
        p = ifa.ifa_next;
        // SAFETY: a node's name is a C string.
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        let name = name.split(':').next().unwrap_or_default(); // Linux lists eth0's aliases as eth0:1
        let i = ifs.iter().position(|i| i.name == name).unwrap_or_else(|| {
            let f = ifa.ifa_flags as c_int;
            ifs.push(Iface {
                // SAFETY: as above.
                index: unsafe { libc::if_nametoindex(ifa.ifa_name) },
                name: name.into(),
                up: f & (libc::IFF_UP | libc::IFF_RUNNING) == libc::IFF_UP | libc::IFF_RUNNING,
                p2p: f & libc::IFF_POINTOPOINT != 0,
                mtu: ifreq(name, libc::SIOCGIFMTU).unwrap_or(0),
                wifi: super::os::wifi(name),
                ..Iface::default()
            });
            ifs.len() - 1
        });
        let (ifc, sa) = (&mut ifs[i], ifa.ifa_addr);
        if sa.is_null() {
            continue;
        }
        // SAFETY: a node's address is a sockaddr of the family it names: an IP one is read whole,
        // unaligned, and of a link-layer one, which may be shorter than its struct, one field.
        unsafe {
            match c_int::from((*sa).sa_family) {
                libc::AF_INET => {
                    let a = sa.cast::<libc::sockaddr_in>().read_unaligned().sin_addr;
                    ifc.addrs.push(a.s_addr.to_ne_bytes().into());
                }
                libc::AF_INET6 => {
                    let a = sa.cast::<libc::sockaddr_in6>().read_unaligned().sin6_addr;
                    ifc.addrs.push(IpAddr::from(a.s6_addr).to_canonical());
                }
                #[cfg(target_os = "linux")]
                libc::AF_PACKET => ifc.mac = (*sa.cast::<libc::sockaddr_ll>()).sll_halen == 6,
                #[cfg(target_os = "macos")]
                libc::AF_LINK => ifc.mac = (*sa.cast::<libc::sockaddr_dl>()).sdl_alen == 6,
                _ => {}
            }
        }
    }
    // SAFETY: head came from getifaddrs, and nothing refers to the list any more.
    unsafe { libc::freeifaddrs(head) };
    Ok(ifs)
}

/// An ioctl on a struct ifreq naming interface `name` that returns an int in the struct's union,
/// such as SIOCGIFMTU.
pub(super) fn ifreq(name: &str, req: libc::c_ulong) -> Option<u32> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, None).ok()?;
    let mut ifr = [0u8; 64]; // more than sizeof(struct ifreq): the name, then the union at 16
    let n = name.len().min(15);
    ifr[..n].copy_from_slice(&name.as_bytes()[..n]);
    // SAFETY: the ioctl reads and writes at most sizeof(struct ifreq) bytes of ifr. The request's
    // type is c_ulong with glibc and macOS, c_int with musl (the release build): `as _`.
    let ok = unsafe { libc::ioctl(s.as_raw_fd(), req as _, ifr.as_mut_ptr()) } == 0;
    ok.then(|| u32::from_ne_bytes([ifr[16], ifr[17], ifr[18], ifr[19]]))
}

/// setsockopt with the option value's bytes.
pub(super) fn setopt(s: &Socket, level: c_int, opt: c_int, v: &[u8]) -> io::Result<()> {
    let len = v.len() as libc::socklen_t;
    // SAFETY: v is valid for reads of len bytes during the call.
    cvt(unsafe { libc::setsockopt(s.as_raw_fd(), level, opt, v.as_ptr().cast(), len) }).map(|_| ())
}

/// A libc call's result: -1 is errno's error.
fn cvt(r: c_int) -> io::Result<c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// Turns off Windows' UDP resets (see windows.rs): here, as in Go, nothing changes.
pub fn no_reset(_: &Socket) {}

/// The sleep-inclusive clock (§8.4): CLOCK_BOOTTIME on Linux, and CLOCK_MONOTONIC on macOS, which
/// counts sleep there. Rust's Instant, like Go's monotonic clock, stops in sleep on both.
pub fn boot() -> i64 {
    let mut ts = MaybeUninit::<libc::timespec>::zeroed();
    // SAFETY: clock_gettime fills the timespec, which is zeroed should it fail.
    let ts = unsafe {
        libc::clock_gettime(super::os::BOOT_CLOCK, ts.as_mut_ptr());
        ts.assume_init()
    };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// This device's host name (status's name, §14.3), as gethostname gives it; "" should it fail.
pub fn hostname() -> String {
    let mut b = [0u8; 256];
    // SAFETY: gethostname writes at most b.len() bytes into b.
    let r = unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) };
    let n = if r == 0 { b.iter().position(|&c| c == 0).unwrap_or(b.len()) } else { 0 };
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// One socket of node's Poll (super::Poll), waited on for something to read.
pub(super) type PollFd = libc::pollfd;
/// One end of the Waker's socket pair.
pub(super) type WakeSock = UnixDatagram;

pub(super) fn poll_fd(s: &impl AsRawFd) -> PollFd {
    libc::pollfd { fd: s.as_raw_fd(), events: libc::POLLIN, revents: 0 }
}

/// Waits until one of fds has something to read or `t` has passed: to the nanosecond on Linux
/// (ppoll), to the next millisecond on macOS (poll), so a deadline is never met early. A signal
/// ends it early.
pub(super) fn poll(fds: &mut [PollFd], t: Duration) -> io::Result<()> {
    let (p, n) = (fds.as_mut_ptr(), fds.len() as libc::nfds_t);
    #[cfg(target_os = "linux")]
    // SAFETY: p points at n pollfds and ts outlives the call; a null mask changes none.
    let r = unsafe {
        let mut ts = MaybeUninit::<libc::timespec>::zeroed().assume_init();
        (ts.tv_sec, ts.tv_nsec) = (t.as_secs().min(3600) as _, t.subsec_nanos() as _);
        libc::ppoll(p, n, &ts, ptr::null())
    };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: p points at n pollfds.
    let r = unsafe { libc::poll(p, n, c_int::try_from(t.as_nanos().div_ceil(1_000_000)).unwrap_or(c_int::MAX)) };
    match cvt(r) {
        Err(e) if e.kind() != io::ErrorKind::Interrupted => Err(e),
        _ => Ok(()),
    }
}

/// The Waker's socket pair, nonblocking.
pub(super) fn wake_pair() -> io::Result<(WakeSock, WakeSock)> {
    let (tx, rx) = UnixDatagram::pair()?;
    tx.set_nonblocking(true)?;
    rx.set_nonblocking(true)?;
    Ok((tx, rx))
}

/// The next datagram waiting on u and its sender, or WouldBlock: MSG_DONTWAIT, since a read after
/// a poll that found u readable can still block (Linux drops a bad checksum only as it reads).
pub fn recv_now(u: &UdpSocket, b: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    // SAFETY: MaybeUninit<u8> is laid out as u8, and recvfrom writes only initialized bytes.
    let b = unsafe { &mut *(b as *mut [u8] as *mut [MaybeUninit<u8>]) };
    let (n, from) = SockRef::from(u).recv_from_with_flags(b, libc::MSG_DONTWAIT)?;
    Ok((n, from.as_socket().ok_or(io::ErrorKind::InvalidData)?))
}

/// Root may install the system service.
pub fn admin() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Runs the daemon directly: launchd and systemd stop it with SIGTERM, whose default action ends
/// the process, so `stop` never fires. First it raises the descriptor limit (`raise_nofile`).
pub fn run_service(run: impl FnOnce(Receiver<()>) -> io::Result<()>) -> io::Result<()> {
    raise_nofile();
    let (_never, stop) = mpsc::channel();
    run(stop)
}

/// Raises the soft RLIMIT_NOFILE to the hard limit, as Go's runtime did at start (review:
/// fd-limit-and-splice-descriptors): launchd starts a job at 256 and systemd a service at 1,024,
/// which forwards, splices, flows and channels can pass. The daemon waits with poll, never select,
/// so descriptors past 1,024 are safe. macOS refuses a soft limit over kern.maxfilesperproc, and
/// launchd's hard limit is unlimited: there it stops at kern.maxfilesperproc.
fn raise_nofile() {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes only the rlimit r is.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return;
    }
    #[cfg(target_os = "macos")]
    let max = {
        let (mut n, mut len): (c_int, usize) = (0, size_of::<c_int>());
        let name = c"kern.maxfilesperproc".as_ptr();
        // SAFETY: name is a C string, and sysctlbyname writes at most len bytes, an int's, into n.
        let got = unsafe { libc::sysctlbyname(name, (&raw mut n).cast(), &mut len, ptr::null_mut(), 0) };
        if got == 0 && n > 0 { r.rlim_max.min(n as libc::rlim_t) } else { r.rlim_max }
    };
    #[cfg(not(target_os = "macos"))]
    let max = r.rlim_max;
    if max > r.rlim_cur {
        r.rlim_cur = max;
        // SAFETY: setrlimit reads only the rlimit r is.
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
    }
}

/// Creates the state directory `dir`, 0755 whatever the umask, so that anyone can reach
/// status.sock in it. As root it then requires that only root can change `dir`, since root runs
/// the update.url config.json names and, on macOS, the executable `dir` holds: `dir` and every
/// directory above it belong to root and are writable by no one else, unless sticky as /tmp is.
pub(super) fn mk_state(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, Permissions::from_mode(0o755))?;
    if !admin() {
        return Ok(());
    }
    for d in fs::canonicalize(dir)?.ancestors() {
        let m = fs::symlink_metadata(d)?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0 {
            let why = "must belong to root and be writable by root alone, since root runs what";
            return Err(io::Error::other(format!("{} {why} {} holds", d.display(), dir.display())));
        }
    }
    Ok(())
}

/// Makes the sockets bound until the guard drops 0600 from their creation (umask 0177).
pub(super) fn private() -> impl Sized {
    struct Restore(libc::mode_t);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: umask only swaps the process's file mode mask.
            unsafe { libc::umask(self.0) };
        }
    }
    // SAFETY: as above.
    Restore(unsafe { libc::umask(0o177) })
}

/// Lets anyone connect to status.sock.
pub(super) fn public(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(0o666))
}

/// A running executable can be replaced; register restarts the daemon.
pub(super) fn halt(_: Scope) {}

/// A running executable can be deleted.
pub(super) fn drop(download: &Path) {
    let _ = fs::remove_file(download);
}

/// The executable and an update's leftovers, `.new` and `.old` beside it.
pub(super) fn remove_exe(exe: &Path) -> io::Result<bool> {
    remove([exe.to_owned(), exe.with_extension("new"), exe.with_extension("old")])
}

/// Makes `path` hold `body`, mode 0644, and reports whether it changed anything.
pub(super) fn write(path: &Path, body: &str) -> io::Result<bool> {
    if fs::read(path).is_ok_and(|b| b == body.as_bytes()) {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().unwrap_or(path))?;
    super::install::put(path, body.as_bytes(), 0o644).map(|()| true)
}

/// Makes `link` a symbolic link to `exe`, the name people type for an executable that sits off
/// PATH (macOS's system one, §22.2), unless something else is there already: that is someone
/// else's, and stays. Returns whether it made the link.
#[cfg(any(target_os = "macos", test))]
pub(super) fn link(link: &Path, exe: &Path) -> io::Result<bool> {
    if link.symlink_metadata().is_ok() {
        return Ok(false);
    }
    fs::create_dir_all(link.parent().unwrap_or(link))?;
    std::os::unix::fs::symlink(exe, link).map(|()| true)
}

/// Removes `link` if it is a symbolic link to `exe`, as [`link`] makes it, and reports whether it
/// did; anything else there stays.
#[cfg(any(target_os = "macos", test))]
pub(super) fn unlink(link: &Path, exe: &Path) -> io::Result<bool> {
    if fs::read_link(link).is_ok_and(|to| to == exe) { remove([link]) } else { Ok(false) }
}

/// Deletes the files that exist and reports whether it deleted any.
pub(super) fn remove<P: AsRef<Path>>(paths: impl IntoIterator<Item = P>) -> io::Result<bool> {
    paths.into_iter().try_fold(false, |changed, p| match fs::remove_file(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        r => Ok(changed | r.is_ok()),
    })
}

/// Runs a command without input and returns its output, or an error with its stderr unless it
/// exits 0. Nothing reaches the terminal, where the CLI may be printing JSON. (Linux's tests fake
/// systemctl, its only caller there.)
#[cfg(any(target_os = "macos", not(test)))]
pub(super) fn run<S: AsRef<std::ffi::OsStr>>(cmd: &str, args: &[S]) -> io::Result<Vec<u8>> {
    let mut c = std::process::Command::new(cmd);
    let out = c.args(args).output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() { Ok(out.stdout) } else { Err(io::Error::other(format!("{c:?}: {}", stderr.trim()))) }
}

/// The launchd job, daemon or agent: started at load, kept alive, no log paths (§15.3).
#[cfg(any(target_os = "macos", test))]
pub(super) fn plist(exe: &Path) -> String {
    let exe = exe.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>com.wirebond</string>
<key>ProgramArguments</key><array><string>{exe}</string><string>run</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>
"#
    )
}
