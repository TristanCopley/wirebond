//! Windows (SPEC §6.2, §8.4, §15.3, §15.5, §15.6): the functions of Go's host_windows.go under the
//! same names, which host/mod.rs calls as `os::<function>`, and what Go's standard library and
//! x/sys gave it here: the interface listing, the UDP resets turned off and the service
//! dispatcher. The local sockets are mod.rs's AF_UNIX ones (Windows 10 1803 and later).
//! notes/rust.md lists where this differs from Go and what is unverified.

use std::ffi::{OsStr, c_void};
use std::fs::OpenOptions;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::os::windows::io::{AsRawHandle, AsRawSocket};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, PoisonError};
use std::{env, slice, thread, time::Duration};

use socket2::{SockRef, Socket};
use windows_sys::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp};
use windows_sys::Win32::Security::{Authorization::*, *};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, REG_EXPAND_SZ, RegGetValueW, RegSetKeyValueW};
use windows_sys::Win32::System::Registry::{RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
use windows_sys::Win32::System::{Services::*, SystemInformation::*, Threading::CREATE_NO_WINDOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};
use windows_sys::Win32::{Foundation::*, Networking::WinSock::*, Storage::FileSystem::*};
use windows_sys::core::{BOOL, PWSTR};

use super::{Iface, Scope};
use wirebond_core::SOCK_BUF;

/// The state directory's owner, administrators, and its DACL: SYSTEM and administrators only,
/// inherited by what is made in it (§15.3).
const STATE_SD: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// status.sock's DACL: the state directory's two ACEs, and read and write, which connecting
/// needs, for every signed-in user.
const PUBLIC_SD: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;AU)";
/// The service's and the firewall rule's name.
const NAME: &str = "wirebond";
/// Where the registry keeps the machine's environment, PATH among it.
const ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// The interfaces as GetAdaptersAddresses lists them, read as Go's net.Interfaces reads them on
/// Windows: the friendly name ("Wi-Fi", "Ethernet 2", "vEthernet (WSL)", "Tailscale"), up when
/// operationally up, point-to-point for PPP, tunnel and ATM, and IPv4-mapped addresses unmapped.
/// Wi-Fi is IfType 71 (IF_TYPE_IEEE80211) in the same listing.
pub fn ifaces() -> io::Result<Vec<Iface>> {
    let (mut buf, mut len) = (Vec::<u64>::new(), 15_000u32); // u64: the records are 8-aligned
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let r = loop {
        buf.resize(len.div_ceil(8) as usize, 0);
        let p = buf.as_mut_ptr().cast();
        // SAFETY: p holds len bytes, aligned for IP_ADAPTER_ADDRESSES_LH; len is in and out.
        match unsafe { GetAdaptersAddresses(AF_UNSPEC.into(), flags, null(), p, &mut len) } {
            ERROR_BUFFER_OVERFLOW => continue,
            r => break r,
        }
    };
    if r == ERROR_NO_DATA {
        return Ok(Vec::new());
    }
    win32(r)?;
    let (mut ifs, mut p) = (Vec::new(), buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>());
    // SAFETY: GetAdaptersAddresses wrote a list of records into buf, which outlives the loop: their
    // links, NUL-terminated names and addresses point into buf, each address is the SOCKADDR_IN or
    // SOCKADDR_IN6 its family says, and IfIndex is the union arm the API fills.
    unsafe {
        while let Some(a) = p.as_ref() {
            let (name, index) = (a.FriendlyName, a.Anonymous1.Anonymous.IfIndex);
            let n = (0..).take_while(|&i| *name.add(i) != 0).count();
            let mut ifc = Iface {
                index: if index == 0 { a.Ipv6IfIndex } else { index }, // 0: IPv4 is off on it
                name: String::from_utf16_lossy(slice::from_raw_parts(name, n)),
                up: a.OperStatus == IfOperStatusUp,
                p2p: matches!(a.IfType, IF_TYPE_PPP | IF_TYPE_TUNNEL | IF_TYPE_ATM),
                mtu: if a.Mtu == u32::MAX { 0 } else { a.Mtu }, // unknown: Go's -1, under 1,500
                mac: a.PhysicalAddressLength == 6,
                wifi: a.IfType == IF_TYPE_IEEE80211,
                addrs: Vec::new(),
            };
            let mut u = a.FirstUnicastAddress;
            while let Some(ua) = u.as_ref() {
                let sa = ua.Address.lpSockaddr;
                match (*sa).sa_family {
                    AF_INET => {
                        let a4 = &*sa.cast::<SOCKADDR_IN>();
                        ifc.addrs.push(a4.sin_addr.S_un.S_addr.to_ne_bytes().into());
                    }
                    AF_INET6 => {
                        let a6 = Ipv6Addr::from((*sa.cast::<SOCKADDR_IN6>()).sin6_addr.u.Byte);
                        ifc.addrs.push(a6.to_canonical());
                    }
                    _ => {}
                }
                u = ua.Next;
            }
            ifs.push(ifc);
            p = a.Next;
        }
    }
    Ok(ifs)
}

/// Pins a link's socket with IP_UNICAST_IF or IPV6_UNICAST_IF, turns the UDP resets off and asks
/// for SOCK_BUF buffers, returning the sizes granted. Windows sends UDP without DF unless asked.
pub fn sockopts(s: &Socket, v6: bool, index: u32, _name: &str) -> io::Result<(usize, usize)> {
    // IPv4 reads the index in network byte order, as an address in 0.0.0.0/8; IPv6 in host order.
    let (level, opt, v) =
        if v6 { (IPPROTO_IPV6, IPV6_UNICAST_IF, index) } else { (IPPROTO_IP, IP_UNICAST_IF, index.to_be()) };
    let p = (&raw const v).cast();
    // SAFETY: s is an open socket and p points at a DWORD that outlives the call.
    if index != 0 && unsafe { setsockopt(s.as_raw_socket() as SOCKET, level, opt, p, 4) } != 0 {
        // SAFETY: WSAGetLastError only reads this thread's last error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    no_reset(s);
    let _ = s.set_recv_buffer_size(SOCK_BUF);
    let _ = s.set_send_buffer_size(SOCK_BUF);
    Ok((s.recv_buffer_size().unwrap_or(0), s.send_buffer_size().unwrap_or(0)))
}

/// Turns off SIO_UDP_CONNRESET and SIO_UDP_NETRESET, so an ICMP port unreachable or TTL expiry
/// does not fail the socket's next read (§6.2: "each UDP socket"). Go turns off the first itself;
/// Rust turns off neither, so node's forward and flow sockets need this too.
pub fn no_reset(s: &Socket) {
    for code in [SIO_UDP_CONNRESET, SIO_UDP_NETRESET] {
        let (off, mut n) = (0u32, 0u32);
        let (input, none) = ((&raw const off).cast(), null_mut());
        // SAFETY: s is an open socket, the input is a 4-byte FALSE and there is no output; with no
        // OVERLAPPED the call completes before it returns.
        unsafe { WSAIoctl(s.as_raw_socket() as SOCKET, code, input, 4, none, 0, &mut n, null_mut(), None) };
    }
}

/// The OS's sleep-inclusive clock (§8.4) in nanoseconds: GetTickCount64, which counts sleep.
pub fn boot() -> i64 {
    // SAFETY: GetTickCount64 takes nothing and cannot fail.
    unsafe { GetTickCount64() as i64 * 1_000_000 }
}

/// This device's host name (status's name, §14.3): its DNS host name, which Go's os.Hostname reads;
/// "" should it fail.
pub fn hostname() -> String {
    let (mut b, mut n) = ([0u16; 256], 256u32);
    // SAFETY: b holds n UTF-16 units; the call writes at most that many and sets n to the name's.
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, b.as_mut_ptr(), &mut n) } != 0;
    if ok { String::from_utf16_lossy(&b[..(n as usize).min(b.len())]) } else { String::new() }
}

/// One socket of node's Poll (super::Poll), waited on for a datagram to read.
pub(super) type PollFd = WSAPOLLFD;
/// One end of the Waker's socket pair.
pub(super) type WakeSock = UdpSocket;

pub(super) fn poll_fd(s: &impl AsRawSocket) -> PollFd {
    WSAPOLLFD { fd: s.as_raw_socket() as SOCKET, events: POLLRDNORM, revents: 0 }
}

/// Waits until one of fds has a datagram to read or `t` has passed, to the next millisecond, so a
/// deadline is never met early. The timeout ends on a timer tick: 1 ms apart in the daemon, which
/// asks for that (`fine_timers`), but often 15.6 ms apart in a process that does not.
pub(super) fn poll(fds: &mut [PollFd], t: Duration) -> io::Result<()> {
    let ms = i32::try_from(t.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX);
    // SAFETY: fds is a slice of WSAPOLLFDs, valid for the call.
    match unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, ms) } {
        SOCKET_ERROR => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// The Waker's socket pair: WSAPoll takes sockets only, so one loopback UDP socket, connected to
/// itself and nonblocking, and its clone.
pub(super) fn wake_pair() -> io::Result<(WakeSock, WakeSock)> {
    let u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    u.connect(u.local_addr()?)?;
    u.set_nonblocking(true)?;
    no_reset(&SockRef::from(&u));
    Ok((u.try_clone()?, u))
}

/// The next datagram waiting on u and its sender, or WouldBlock. Winsock has no MSG_DONTWAIT, so u
/// is nonblocking for the read alone: the loop's sends on it wait for room, as on any OS.
pub fn recv_now(u: &UdpSocket, b: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    u.set_nonblocking(true)?;
    let r = u.recv_from(b);
    u.set_nonblocking(false)?;
    r
}

/// Whether this process may install the system service: it is elevated.
pub fn admin() -> bool {
    let (mut e, mut n) = (TOKEN_ELEVATION::default(), 0);
    let size = size_of_val(&e) as u32;
    let token = -4isize as HANDLE; // GetCurrentProcessToken(): a pseudo-handle, never closed
    let p = (&raw mut e).cast();
    // SAFETY: token has TOKEN_QUERY, and p points at size bytes.
    let got = unsafe { GetTokenInformation(token, TokenElevation, p, size, &mut n) };
    got != 0 && e.TokenIsElevated != 0
}

/// The executable and the state directory (§15.3). Windows has only the system scope.
pub fn paths(_: Scope) -> (PathBuf, PathBuf) {
    let var = |k, d: &str| env::var_os(k).map_or_else(|| d.into(), PathBuf::from);
    let exe = var("ProgramFiles", r"C:\Program Files").join(r"wirebond\wirebond.exe");
    (exe, var("ProgramData", r"C:\ProgramData").join("wirebond"))
}

/// Creates the state directory with STATE_SD from the first moment, so both sockets are
/// admin-only when they appear. Any user may create a folder in %ProgramData%, and its owner
/// could change config.json, whose update.url SYSTEM runs, so one that exists is used only as
/// mk_state makes it: owned by administrators or SYSTEM, with a protected DACL. That is read
/// through a handle that does not follow a link.
pub fn mk_state(dir: &Path) -> io::Result<()> {
    let sd = Sd::new(STATE_SD)?;
    let size = size_of::<SECURITY_ATTRIBUTES>() as u32;
    let sa = SECURITY_ATTRIBUTES { nLength: size, lpSecurityDescriptor: sd.0, bInheritHandle: 0 };
    // SAFETY: the path is NUL-terminated and sa holds a valid descriptor; both outlive the call.
    match ok(unsafe { CreateDirectoryW(wide(dir).as_ptr(), &sa) }) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        r => return r,
    }
    let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    let f = OpenOptions::new().access_mode(READ_CONTROL).custom_flags(flags).open(dir)?;
    let (h, what) = (f.as_raw_handle(), OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION);
    let (mut owner, mut cur, mut ctl, mut rev) = (null_mut(), null_mut(), 0, 0);
    let (g, d, s) = (null_mut(), null_mut(), null_mut()); // group, DACL and SACL: not wanted
    // SAFETY: h is open with READ_CONTROL; GetSecurityInfo allocates cur, which Sd frees after the
    // checks, and owner points into it; ctl and rev are out parameters.
    let ours = unsafe {
        let r = GetSecurityInfo(h, SE_FILE_OBJECT, what, &mut owner, g, d, s, &mut cur);
        let cur = Sd(cur);
        win32(r)?;
        let sid = |kind| IsWellKnownSid(owner, kind) != 0;
        !owner.is_null()
            && (sid(WinBuiltinAdministratorsSid) || sid(WinLocalSystemSid))
            && GetSecurityDescriptorControl(cur.0, &mut ctl, &mut rev) != 0
    };
    if ours && ctl & SE_DACL_PROTECTED != 0 {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{} was not made by wirebond, and whoever made it could change what SYSTEM runs: delete \
         it, then run the same command again",
        dir.display()
    )))
}

/// The state directory's DACL covers both sockets: nothing to narrow, nothing to restore.
pub fn private() -> impl Sized {}

/// Lets every signed-in user connect to status.sock, which needs write access to the file. The
/// socket file is a reparse point that no file system handles, so it is opened as one and given
/// its whole DACL; SetNamedSecurityInfo, which Go uses, would follow it.
pub fn public(path: &Path) -> io::Result<()> {
    let sd = Sd::new(PUBLIC_SD)?;
    let f = OpenOptions::new().access_mode(WRITE_DAC).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    // SAFETY: f is open with WRITE_DAC and sd is a valid descriptor; both outlive the call.
    ok(unsafe { SetKernelObjectSecurity(f.as_raw_handle(), DACL_SECURITY_INFORMATION, sd.0) })
}

/// A security descriptor the system allocated, freed with LocalFree.
struct Sd(PSECURITY_DESCRIPTOR);

impl Sd {
    /// The descriptor an SDDL string describes.
    fn new(sddl: &str) -> io::Result<Sd> {
        let (s, mut sd, rev) = (wide(sddl), null_mut(), SDDL_REVISION_1);
        let (p, size) = (s.as_ptr(), null_mut()); // the size is not wanted
        // SAFETY: p is NUL-terminated and sd is an out parameter.
        let r = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(p, rev, &mut sd, size) };
        ok(r).map(|()| Sd(sd))
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: the system allocated the descriptor with LocalAlloc, or it is null, and only
        // this frees it.
        unsafe { LocalFree(self.0) };
    }
}

/// Runs one firewall command line as written, so program="<exe>" keeps its quotes. netsh prints
/// its errors on standard output, which becomes the error.
fn netsh(args: &str) -> io::Result<()> {
    let line = format!("advfirewall firewall {args}");
    let out = Command::new("netsh").raw_arg(&line).output()?;
    let msg = String::from_utf8_lossy(&out.stdout);
    if out.status.success() { Ok(()) } else { Err(io::Error::other(format!("netsh {line}: {}", msg.trim()))) }
}

/// Puts `dir` on the machine's PATH (`on`) or takes it off, every other entry kept as stored,
/// %SystemRoot% unexpanded; whether that changed it. Programs started from then on find
/// `wirebond` there: Explorer, told of the change as setx tells it, passes the new PATH on.
fn machine_path(dir: &Path, on: bool) -> io::Result<bool> {
    let (key, name) = (wide(ENVIRONMENT), wide("Path"));
    // A machine without a PATH value has an empty one.
    let old = match reg_string(&key, &name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        r => r?,
    };
    let dir = dir.to_string_lossy();
    let new = if on { super::install::path_with(&old, &dir) } else { super::install::path_without(&old, &dir) };
    let Some(new) = new else { return Ok(false) };
    let data = wide(&new);
    let len = u32::try_from(data.len() * 2).map_err(io::Error::other)?;
    // SAFETY: key and name are NUL-terminated, and data holds len bytes.
    win32(unsafe {
        RegSetKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), REG_EXPAND_SZ, data.as_ptr().cast(), len)
    })?;
    let what = wide("Environment");
    // SAFETY: what is NUL-terminated and outlives the call, which gives each window at most 1 s.
    unsafe {
        let lparam = what.as_ptr() as isize;
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, lparam, SMTO_ABORTIFHUNG, 1000, null_mut())
    };
    Ok(true)
}

/// The string value `name` of the key HKLM\`key`, unexpanded.
fn reg_string(key: &[u16], name: &[u16]) -> io::Result<String> {
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let (mut buf, mut len) = (Vec::<u16>::new(), 0u32);
    loop {
        let p = if buf.is_empty() { null_mut() } else { buf.as_mut_ptr().cast() };
        // SAFETY: key and name are NUL-terminated; p is null, which asks for the size alone, or
        // holds len bytes; len is in and out.
        match unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), flags, null_mut(), p, &mut len) } {
            ERROR_MORE_DATA => {} // it grew since the size was asked
            0 if !buf.is_empty() => break,
            r => win32(r)?,
        }
        buf.resize((len as usize).div_ceil(2).max(1), 0);
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..n]))
}

/// Adds the firewall rule and the service (automatic, LocalSystem, restarted 1 s after each
/// failure, the count reset after a day, an error exit counting as a failure) and starts it, and
/// puts the executable's folder on the machine's PATH; without that, which only costs the full
/// path, the CLI says how to run it.
pub fn register(_: Scope, exe: &Path, _restart: bool) -> io::Result<bool> {
    let mut changed = netsh("show rule name=wirebond").is_err();
    if changed {
        let (rule, exe) = ("name=wirebond dir=in action=allow", exe.display());
        netsh(&format!(r#"add rule {rule} program="{exe}" enable=yes profile=any"#))?;
    }
    // After the rule, whose absence alone decides whether to add it.
    changed |= exe.parent().is_some_and(|d| machine_path(d, true).unwrap_or(false));
    // wintun.dll beside the executable (SPEC 2.0 §5.3), from the copy embedded in it.
    if v2::wintun_embedded() {
        changed |= v2::place_wintun(exe)?;
    }
    let m = Sc::manager()?;
    let s = match m.service() {
        Ok(s) => s,
        Err(_) => {
            changed = true;
            let (name, line) = (wide(NAME), wide(format!(r#""{}" run"#, exe.display())));
            // SAFETY: m is open with every right, and name and line are NUL-terminated.
            let h = unsafe {
                CreateServiceW(
                    m.0,
                    name.as_ptr(),
                    null(), // display name: the name
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    line.as_ptr(),
                    null(),     // no load order group
                    null_mut(), // no tag
                    null(),     // no dependencies
                    null(),     // LocalSystem
                    null(),     // no password
                )
            };
            Sc::new(h)?
        }
    };
    let mut restart = [SC_ACTION { Type: SC_ACTION_RESTART, Delay: 1000 }; 3];
    let actions = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        cActions: 3,
        lpsaActions: restart.as_mut_ptr(),
        ..Default::default()
    };
    let flag = SERVICE_FAILURE_ACTIONS_FLAG { fFailureActionsOnNonCrashFailures: 1 };
    let (actions, flag) = ((&raw const actions).cast(), (&raw const flag).cast());
    // SAFETY: s is open with every right; the structures, and the array actions points to, outlive
    // the calls.
    unsafe {
        let set = |level, info| ok(ChangeServiceConfig2W(s.0, level, info));
        set(SERVICE_CONFIG_FAILURE_ACTIONS, actions)?;
        set(SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, flag)?;
        match ok(StartServiceW(s.0, 0, null())) {
            Err(e) if e.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) => Err(e),
            _ => Ok(changed),
        }
    }
}

/// Stops the service, since Windows can neither replace nor delete a running executable.
pub fn halt(_: Scope) {
    let _ = stop_service(false);
}

/// Removes the firewall rule and the PATH entry, then stops and deletes the service. A PATH entry
/// it cannot remove is the note, for the CLI to print.
pub fn unregister(_: Scope, exe: &Path) -> io::Result<(bool, String)> {
    let mut changed = netsh("delete rule name=wirebond").is_ok();
    let mut note = String::new();
    if let Some(d) = exe.parent() {
        match machine_path(d, false) {
            Ok(c) => changed |= c,
            Err(e) => note = format!("remove {} from the system PATH ({e})", d.display()),
        }
    }
    Ok((stop_service(true)? || changed, note))
}

/// Stops the service, if there is one, waiting up to 10 s, then deletes it if del. Returns
/// whether there was one.
fn stop_service(del: bool) -> io::Result<bool> {
    let m = Sc::manager()?;
    let Ok(s) = m.service() else { return Ok(false) };
    let mut st = SERVICE_STATUS::default();
    // SAFETY: s is open with every right and st is an out parameter. Stop fails when the service
    // is stopped or stopping already; either way, wait for stopped.
    unsafe { ControlService(s.0, SERVICE_CONTROL_STOP, &mut st) };
    for _ in 0..100 {
        // SAFETY: as above.
        let queried = unsafe { QueryServiceStatus(s.0, &mut st) } != 0;
        if !queried || st.dwCurrentState == SERVICE_STOPPED {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if del {
        // SAFETY: s is open with every right, DELETE included.
        ok(unsafe { DeleteService(s.0) })?;
    }
    Ok(true)
}

/// A service manager or service handle, closed on drop.
struct Sc(SC_HANDLE);

impl Sc {
    fn new(h: SC_HANDLE) -> io::Result<Sc> {
        let err = io::Error::last_os_error();
        if h.is_null() { Err(err) } else { Ok(Sc(h)) }
    }

    /// This machine's service manager, with every right: an administrator's.
    fn manager() -> io::Result<Sc> {
        // SAFETY: null names mean this machine's active services database.
        Sc::new(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ALL_ACCESS) })
    }

    /// The wirebond service, with every right.
    fn service(&self) -> io::Result<Sc> {
        let name = wide(NAME);
        // SAFETY: self is an open manager handle and name is NUL-terminated.
        Sc::new(unsafe { OpenServiceW(self.0, name.as_ptr(), SERVICE_ALL_ACCESS) })
    }
}

impl Drop for Sc {
    fn drop(&mut self) {
        // SAFETY: the handle is open, and only this closes it.
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Runs line in a detached cmd about 2 s from now, once this process has exited: Windows deletes
/// neither a running executable nor a directory it runs in. cmd starts in %SystemRoot%, so it
/// keeps no directory it deletes open, and holds none of the CLI's standard handles, so whatever
/// reads the CLI's output does not wait for it.
fn later(line: &str) -> io::Result<()> {
    let root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let mut c = Command::new("cmd");
    c.raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & {line}"));
    c.current_dir(root).creation_flags(CREATE_NO_WINDOW);
    c.stdin(Stdio::null()).stdout(Stdio::null());
    c.stderr(Stdio::null()).spawn().map(|_detached| ())
}

/// Deletes the download, which is this process's executable, once this process has exited.
pub fn drop(download: &Path) {
    let _ = later(&format!(r#"del /f /q "{}""#, download.display()));
}

/// Deletes the executable's directory, update leftovers included, once this process has exited
/// (§15.6).
pub fn remove_exe(exe: &Path) -> io::Result<bool> {
    let Some(dir) = exe.parent().filter(|d| d.exists()) else {
        return Ok(false);
    };
    later(&format!(r#"rmdir /s /q "{}""#, dir.display())).map(|()| true)
}

/// What run_service shares with the service manager's callbacks, which carry no context: where
/// ServiceMain reports, the status handle, and the stop sender, which the first stop takes.
static READY: Mutex<Option<Sender<io::Result<()>>>> = Mutex::new(None);
static STATUS: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static STOP: Mutex<Option<Sender<()>>> = Mutex::new(None);

fn put<T>(m: &Mutex<Option<T>>, v: Option<T>) -> Option<T> {
    std::mem::replace(&mut *m.lock().unwrap_or_else(PoisonError::into_inner), v)
}

/// Runs the daemon, `wirebond run`. Started by the service manager, it reports running and fires
/// stop on a stop or shutdown request; otherwise it calls run, and stop never fires. The
/// manager's dispatcher, which calls back into this process, runs on a thread of its own, so run
/// stays on the caller's. First it asks for 1 ms timers (`fine_timers`).
pub fn run_service(run: impl FnOnce(Receiver<()>) -> io::Result<()>) -> io::Result<()> {
    let _ms = fine_timers();
    let ((ready, started), (stop, stopped)) = (mpsc::channel(), mpsc::channel());
    put(&READY, Some(ready.clone()));
    put(&STOP, Some(stop));
    let dispatcher = thread::spawn(move || {
        let name = wide(NAME);
        let main = SERVICE_TABLE_ENTRYW { lpServiceName: name.as_ptr().cast_mut(), lpServiceProc: Some(service_main) };
        let table = [main, SERVICE_TABLE_ENTRYW::default()];
        // SAFETY: the table ends in a null entry and outlives the call, which returns once the
        // service has stopped.
        if let Err(e) = ok(unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }) {
            let _ = ready.send(Err(e));
        }
    });
    let by_hand = Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT as i32);
    match started.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.raw_os_error() != by_hand => return Err(e),
        _ => return run(stopped), // STOP keeps its sender, so stop never fires
    }
    let r = run(stopped);
    // Exit code 1 when the daemon ended unasked, so the recovery actions restart it, and 0 after
    // a stop request, so a stop sticks. An update's exit(3) ends the process without reporting
    // SERVICE_STOPPED, which they count as a failure too (§15.5).
    status(SERVICE_STOPPED, 0, put(&STOP, None).is_some().into());
    let _ = dispatcher.join();
    r
}

/// Asks for 1 ms timers until the guard drops or the process exits (review: windows timer). Since
/// Windows 10 2004 a process that does not ask is guaranteed no finer timers than the default tick,
/// often 15.6 ms, whatever other processes ask: node's loop, waiting in WSAPoll between frames for
/// the pacer's next tick, a reorder timeout or a probe deadline, would wake up to 15 ms late. Go's
/// runtime woke its poller with a high-resolution waitable timer, which WSAPoll cannot wait on;
/// Sunshine asks for 1 ms or finer too. Microsoft warns that finer timers cost power. node's daemon
/// tests ask too: they step the daemon through WSAPoll with 2 ms waits.
pub fn fine_timers() -> impl Sized {
    struct End;
    impl Drop for End {
        fn drop(&mut self) {
            // SAFETY: timeEndPeriod takes a number and touches no memory. An End exists only once
            // the request below has succeeded, and each request that succeeds must be ended.
            unsafe { timeEndPeriod(1) };
        }
    }
    // SAFETY: timeBeginPeriod takes a number and touches no memory.
    if unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR { Some(End) } else { None }
}

/// ServiceMain: registers the control handler, reports running and tells run_service, then
/// returns, which ends this thread but not the service.
extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let name = wide(NAME);
    // SAFETY: name is NUL-terminated and handler lives as long as the process.
    let h = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), null()) };
    let err = io::Error::last_os_error();
    STATUS.store(h, Ordering::Release);
    let r = if h.is_null() { Err(err) } else { Ok(()) };
    if r.is_ok() {
        status(SERVICE_RUNNING, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0);
    }
    if let Some(ready) = put(&READY, None) {
        let _ = ready.send(r);
    }
}

/// HandlerEx: the first stop or shutdown request reports stop pending, which accepts no further
/// request, and fires stop; a second, which can arrive before that lands, does nothing.
/// Interrogate needs no answer: the manager knows the status.
extern "system" fn handler(ctl: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match ctl {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            if let Some(stop) = put(&STOP, None) {
                status(SERVICE_STOP_PENDING, 0, 0);
                let _ = stop.send(());
            }
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Reports the service's state to the manager.
fn status(state: u32, accepts: u32, code: u32) {
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepts,
        dwWin32ExitCode: code,
        ..Default::default()
    };
    // SAFETY: STATUS holds the handle RegisterServiceCtrlHandlerExW returned, which stays valid,
    // and st outlives the call.
    unsafe { SetServiceStatus(STATUS.load(Ordering::Acquire), &st) };
}

/// s as a NUL-terminated UTF-16 string.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

/// A BOOL result: false is GetLastError's error.
fn ok(b: BOOL) -> io::Result<()> {
    let err = io::Error::last_os_error();
    if b != 0 { Ok(()) } else { Err(err) }
}

/// A WIN32_ERROR result.
fn win32(e: u32) -> io::Result<()> {
    let err = io::Error::from_raw_os_error(e as i32);
    if e == 0 { Ok(()) } else { Err(err) }
}

#[cfg(test)]
#[path = "windows/tests.rs"]
mod tests;

// ---------------------------------------------------------------------------------------------
// The 2.0 platform API (SPEC 2.0 §5–§8; host/net.rs has the types and the contract). Owner: host
// macos and windows.

pub use v2::*;

#[allow(dead_code)] // 2.0 wave 2: the daemon's pumps and bond logic call these
mod v2 {
    use std::ffi::c_void;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawSocket;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use std::time::Duration;
    use std::{env, mem, slice};

    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::NetworkManagement::IpHelper::*;
    use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows_sys::Win32::Networking::WinSock::*;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::core::GUID;

    use super::super::net::{Capture, Egress, EgressOn, Routes, Share, TunConfig};
    use super::{CREATE_NO_WINDOW, wide, win32};

    /// The adapter's name, tunnel type and fixed GUID: Windows keeps one network profile for it.
    const NAME: &str = "wirebond";
    const ADAPTER_GUID: GUID = GUID::from_u128(0x7849b0d0_2e0f_4a7c_9d3e_77b0d0007849);
    /// Wintun's ring: a power of two between 128 KiB and 64 MiB.
    const RING: u32 = 0x40_0000;
    /// The interface metric (§5.3, §7): below every physical link's, so its DNS server is asked first.
    const METRIC: u32 = 5;
    /// What Windows ICS gives the shared adapter (§6.2): it readdresses it to 192.168.137.1/24.
    const ICS_ADDR: (Ipv4Addr, u8) = (Ipv4Addr::new(192, 168, 137, 1), 24);
    /// Wintun 0.14.1's wintun.dll for this architecture, from the zip crates/wirebond/wintun.sum pins
    /// (build.rs; .github/scripts/wintun.sh downloads and checks it). Empty in a build without it.
    #[cfg(wintun)]
    const WINTUN_DLL: &[u8] = include_bytes!(env!("WIREBOND_WINTUN_DLL"));
    #[cfg(not(wintun))]
    const WINTUN_DLL: &[u8] = &[];

    type Adapter = *mut c_void;
    type Session = *mut c_void;

    /// Wintun's entry points (wintun.h, 0.14).
    struct Wintun {
        create: unsafe extern "system" fn(*const u16, *const u16, *const GUID) -> Adapter,
        close: unsafe extern "system" fn(Adapter),
        luid: unsafe extern "system" fn(Adapter, *mut NET_LUID_LH),
        start: unsafe extern "system" fn(Adapter, u32) -> Session,
        end: unsafe extern "system" fn(Session),
        event: unsafe extern "system" fn(Session) -> HANDLE,
        receive: unsafe extern "system" fn(Session, *mut u32) -> *mut u8,
        release: unsafe extern "system" fn(Session, *const u8),
        allocate: unsafe extern "system" fn(Session, u32) -> *mut u8,
        send: unsafe extern "system" fn(Session, *const u8),
    }

    /// wintun.dll beside the executable, written from the embedded copy where missing or different,
    /// checked against the embedded copy's SHA-256 before it loads, loaded once a process.
    fn wintun() -> io::Result<&'static Wintun> {
        static LIB: OnceLock<Result<Wintun, String>> = OnceLock::new();
        let lib = LIB.get_or_init(|| load().map_err(|e| e.to_string()));
        lib.as_ref().map_err(|e| io::Error::other(e.clone()))
    }

    /// Where wintun.dll goes: beside the running executable (§5.3).
    pub fn wintun_path() -> io::Result<PathBuf> {
        let exe = env::current_exe()?;
        Ok(exe.parent().unwrap_or(Path::new(".")).join("wintun.dll"))
    }

    /// Writes wintun.dll beside `exe` unless it is there already, byte for byte: install calls it
    /// with the installed executable (§5.3). Returns whether it wrote.
    pub fn place_wintun(exe: &Path) -> io::Result<bool> {
        if WINTUN_DLL.is_empty() {
            return Err(no_wintun());
        }
        let path = exe.parent().unwrap_or(Path::new(".")).join("wintun.dll");
        if fs::read(&path).is_ok_and(|b| b == WINTUN_DLL) {
            return Ok(false);
        }
        super::super::install::put(&path, WINTUN_DLL, 0o644).map(|()| true)
    }

    /// Whether this build embeds wintun.dll: every release build for Windows does (build.rs).
    pub fn wintun_embedded() -> bool {
        !WINTUN_DLL.is_empty()
    }

    fn no_wintun() -> io::Error {
        io::Error::other(
            "this build has no wintun.dll embedded: run .github/scripts/wintun.sh, which downloads Wintun \
             0.14.1 and checks it against crates/wirebond/wintun.sum, then build again",
        )
    }

    #[allow(clippy::missing_transmute_annotations)] // each target type is the field's, FARPROC's the source
    fn load() -> io::Result<Wintun> {
        let path = wintun_path()?;
        let exe = env::current_exe()?;
        place_wintun(&exe)?;
        // Opened without write or delete sharing, so nothing can change the file between the check and
        // the load, which shares it for reading.
        let mut f = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&path)?;
        let mut b = Vec::with_capacity(WINTUN_DLL.len());
        f.read_to_end(&mut b)?;
        if Sha256::digest(&b) != Sha256::digest(WINTUN_DLL) {
            let msg = format!("{} does not match the embedded Wintun's SHA-256", path.display());
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
        // SAFETY: the path is NUL-terminated; the module stays loaded for the process's life.
        let m = unsafe { LoadLibraryW(wide(&path).as_ptr()) };
        if m.is_null() {
            return Err(io::Error::last_os_error());
        }
        drop(f);
        let get = |name: &str| {
            let c: Vec<u8> = name.bytes().chain([0]).collect();
            // SAFETY: m is a loaded module and c is NUL-terminated.
            unsafe { GetProcAddress(m, c.as_ptr()) }
                .ok_or_else(|| io::Error::other(format!("wintun.dll has no {name}")))
        };
        // SAFETY: each entry point has the signature wintun.h 0.14 declares for it, WINAPI being
        // "system".
        unsafe {
            Ok(Wintun {
                create: mem::transmute(get("WintunCreateAdapter")?),
                close: mem::transmute(get("WintunCloseAdapter")?),
                luid: mem::transmute(get("WintunGetAdapterLUID")?),
                start: mem::transmute(get("WintunStartSession")?),
                end: mem::transmute(get("WintunEndSession")?),
                event: mem::transmute(get("WintunGetReadWaitEvent")?),
                receive: mem::transmute(get("WintunReceivePacket")?),
                release: mem::transmute(get("WintunReleaseReceivePacket")?),
                allocate: mem::transmute(get("WintunAllocateSendPacket")?),
                send: mem::transmute(get("WintunSendPacket")?),
            })
        }
    }

    /// A Wintun 0.14.1 adapter named `wirebond`, interface metric 5, and its session; wintun.dll
    /// is embedded, written beside the executable and checked against its SHA-256 before every
    /// load (§5.3). Closing it removes the adapter, and with it its addresses and routes.
    pub struct Tun {
        name: String,
        lib: &'static Wintun,
        adapter: Adapter,
        session: Session,
        /// Wintun's read-wait event, one of the three handles the loop waits on (§4.3).
        event: HANDLE,
        luid: NET_LUID_LH,
    }

    // SAFETY: Wintun's session calls may come from any thread; the loop uses one at a time.
    unsafe impl Send for Tun {}

    impl Tun {
        /// Creates the adapter, gives it the MTU, metric 5 and the addresses, and starts its session.
        pub fn open(c: &TunConfig) -> io::Result<Tun> {
            let lib = wintun()?;
            let (name, kind) = (wide(&c.name), wide(NAME));
            // SAFETY: both strings are NUL-terminated and the GUID outlives the call.
            let adapter = unsafe { (lib.create)(name.as_ptr(), kind.as_ptr(), &ADAPTER_GUID) };
            if adapter.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut luid = NET_LUID_LH { Value: 0 };
            // SAFETY: adapter is open and luid an out parameter.
            unsafe { (lib.luid)(adapter, &mut luid) };
            // SAFETY: adapter is open; RING is a power of two in Wintun's range.
            let session = unsafe { (lib.start)(adapter, RING) };
            if session.is_null() {
                let e = io::Error::last_os_error();
                // SAFETY: adapter is open and closed only here.
                unsafe { (lib.close)(adapter) };
                return Err(e);
            }
            // SAFETY: session is started.
            let event = unsafe { (lib.event)(session) };
            let tun = Tun { name: c.name.clone(), lib, adapter, session, event, luid };
            if let Ok(g) = guid_of(&tun.luid) {
                let _ = (dns(g, false, None), dns(g, true, None)); // none left from a crash (§5.4)
            }
            for v6 in [false, true] {
                interface(&tun.luid, v6, c.mtu as u32).or_else(|e| if v6 { Ok(()) } else { Err(e) })?;
            }
            if let Some((a, bits)) = c.addr4 {
                address(&tun.luid, IpAddr::V4(a), bits)?;
            }
            if let Some((a, bits)) = c.addr6 {
                address(&tun.luid, IpAddr::V6(a), bits)?;
            }
            Ok(tun)
        }
        pub fn name(&self) -> &str {
            &self.name
        }
        /// One packet into `buf`; WouldBlock when none waits. A packet longer than `buf` is dropped.
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut n = 0u32;
            // SAFETY: the session is started; n is an out parameter.
            let p = unsafe { (self.lib.receive)(self.session, &mut n) };
            if p.is_null() {
                let e = io::Error::last_os_error();
                return Err(match e.raw_os_error() {
                    Some(c) if c == ERROR_NO_MORE_ITEMS as i32 => io::ErrorKind::WouldBlock.into(),
                    _ => e,
                });
            }
            let n = n as usize;
            let fits = n <= buf.len();
            if fits {
                // SAFETY: Wintun gave n bytes at p, valid until released below.
                buf[..n].copy_from_slice(unsafe { slice::from_raw_parts(p, n) });
            }
            // SAFETY: p came from WintunReceivePacket on this session and is released once.
            unsafe { (self.lib.release)(self.session, p) };
            if fits { Ok(n) } else { Err(io::Error::new(io::ErrorKind::InvalidData, "packet longer than the buffer")) }
        }
        /// One packet; WouldBlock when Wintun's ring is full.
        pub fn write(&self, packet: &[u8]) -> io::Result<()> {
            // SAFETY: the session is started.
            let p = unsafe { (self.lib.allocate)(self.session, packet.len() as u32) };
            if p.is_null() {
                let e = io::Error::last_os_error();
                return Err(match e.raw_os_error() {
                    Some(c) if c == ERROR_BUFFER_OVERFLOW as i32 => io::ErrorKind::WouldBlock.into(),
                    _ => e,
                });
            }
            // SAFETY: Wintun allocated packet.len() bytes at p, which WintunSendPacket then takes.
            unsafe {
                p.copy_from_nonoverlapping(packet.as_ptr(), packet.len());
                (self.lib.send)(self.session, p);
            }
            Ok(())
        }
        /// Signalled when a packet waits: WaitForMultipleObjects with the sockets' WSAEventSelect
        /// event and the waitable timer (§4.3, [`EventLoop`]).
        pub fn read_event(&self) -> HANDLE {
            self.event
        }
    }

    impl Drop for Tun {
        fn drop(&mut self) {
            // SAFETY: the session and the adapter are open, and only this ends and closes them.
            unsafe {
                (self.lib.end)(self.session);
                (self.lib.close)(self.adapter);
            }
        }
    }

    /// Sets the adapter's MTU and metric 5 for one family.
    fn interface(luid: &NET_LUID_LH, v6: bool, mtu: u32) -> io::Result<()> {
        // SAFETY: MIB_IPINTERFACE_ROW is plain data; Initialize fills its defaults.
        let mut row: MIB_IPINTERFACE_ROW = unsafe { mem::zeroed() };
        // SAFETY: row is a valid row.
        unsafe { InitializeIpInterfaceEntry(&mut row) };
        row.Family = if v6 { AF_INET6 } else { AF_INET };
        row.InterfaceLuid = *luid;
        // SAFETY: as above; Get fills the rest from the interface.
        win32(unsafe { GetIpInterfaceEntry(&mut row) })?;
        row.NlMtu = mtu;
        row.UseAutomaticMetric = false;
        row.Metric = METRIC;
        row.SitePrefixLength = 0; // Set refuses the value Get returns for IPv4
        // SAFETY: as above.
        win32(unsafe { SetIpInterfaceEntry(&mut row) })
    }

    /// Adds `addr`/`bits` to the adapter, which also routes the subnet into it.
    fn address(luid: &NET_LUID_LH, addr: IpAddr, bits: u8) -> io::Result<()> {
        // SAFETY: as in `interface`.
        let mut row: MIB_UNICASTIPADDRESS_ROW = unsafe { mem::zeroed() };
        // SAFETY: row is a valid row.
        unsafe { InitializeUnicastIpAddressEntry(&mut row) };
        row.InterfaceLuid = *luid;
        row.Address = inet(addr);
        row.OnLinkPrefixLength = bits;
        row.DadState = IpDadStatePreferred; // no duplicate detection on a point-to-point device
        // SAFETY: as above.
        match unsafe { CreateUnicastIpAddressEntry(&row) } {
            ERROR_OBJECT_ALREADY_EXISTS => Ok(()),
            e => win32(e),
        }
    }

    fn inet(a: IpAddr) -> SOCKADDR_INET {
        // SAFETY: SOCKADDR_INET is plain data, all zeros the unspecified address.
        let mut s: SOCKADDR_INET = unsafe { mem::zeroed() };
        match a {
            IpAddr::V4(a) => {
                s.Ipv4.sin_family = AF_INET;
                s.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(a.octets());
            }
            IpAddr::V6(a) => {
                s.Ipv6.sin6_family = AF_INET6;
                s.Ipv6.sin6_addr.u.Byte = a.octets();
            }
        }
        s
    }

    /// The four /1 routes (§5.3): they cover the default without replacing it.
    fn halves() -> [(IpAddr, u8); 4] {
        let (v4, v6) =
            (|b| IpAddr::V4(Ipv4Addr::new(b, 0, 0, 0)), |s| IpAddr::V6(Ipv6Addr::new(s, 0, 0, 0, 0, 0, 0, 0)));
        [(v4(0), 1), (v4(128), 1), (v6(0), 1), (v6(0x8000), 1)]
    }

    /// The adapter named `name`'s LUID, or None when there is none.
    fn luid_of(name: &str) -> Option<NET_LUID_LH> {
        let mut luid = NET_LUID_LH { Value: 0 };
        // SAFETY: the name is NUL-terminated and luid an out parameter.
        (unsafe { ConvertInterfaceAliasToLuid(wide(name).as_ptr(), &mut luid) } == 0).then_some(luid)
    }

    /// Adds or deletes one on-link route through the adapter; one that exists, or is gone, is fine.
    fn route(luid: &NET_LUID_LH, (net, bits): (IpAddr, u8), add: bool) -> io::Result<()> {
        // SAFETY: as in `interface`.
        let mut row: MIB_IPFORWARD_ROW2 = unsafe { mem::zeroed() };
        // SAFETY: row is a valid row.
        unsafe { InitializeIpForwardEntry(&mut row) };
        row.InterfaceLuid = *luid;
        row.DestinationPrefix = IP_ADDRESS_PREFIX { Prefix: inet(net), PrefixLength: bits };
        let none = if net.is_ipv4() { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V6(Ipv6Addr::UNSPECIFIED) };
        row.NextHop = inet(none); // on-link
        row.Metric = 0;
        row.Protocol = MIB_IPPROTO_NETMGMT;
        // SAFETY: row is a valid row.
        match unsafe { if add { CreateIpForwardEntry2(&row) } else { DeleteIpForwardEntry2(&row) } } {
            ERROR_OBJECT_ALREADY_EXISTS | ERROR_NOT_FOUND => Ok(()),
            e => win32(e),
        }
    }

    /// The adapter's interface GUID, which SetInterfaceDnsSettings takes.
    fn guid_of(luid: &NET_LUID_LH) -> io::Result<GUID> {
        let mut g = GUID::from_u128(0);
        // SAFETY: luid is valid and g an out parameter.
        win32(unsafe { ConvertInterfaceLuidToGuid(luid, &mut g) }).map(|()| g)
    }

    /// Sets (or with None clears) the adapter's DNS server for one family (Windows 10 2004 and later).
    fn dns(guid: GUID, v6: bool, server: Option<IpAddr>) -> io::Result<()> {
        let mut name = wide(server.map(|a| a.to_string()).unwrap_or_default());
        let flags = DNS_SETTING_NAMESERVER as u64 | if v6 { DNS_SETTING_IPV6 as u64 } else { 0 };
        let s = DNS_INTERFACE_SETTINGS {
            Version: DNS_INTERFACE_SETTINGS_VERSION1,
            Flags: flags,
            NameServer: name.as_mut_ptr(),
            Domain: null_mut(),
            SearchList: null_mut(),
            RegistrationEnabled: 0,
            RegisterAdapterName: 0,
            EnableLLMNR: 0,
            QueryAdapterName: 0,
            ProfileNameServer: null_mut(),
        };
        // SAFETY: s and the string it points to outlive the call.
        win32(unsafe { SetInterfaceDnsSettings(guid, &s) })
    }

    /// 0.0.0.0/1, 128.0.0.0/1, ::/1 and 8000::/1 through CreateIpForwardEntry2, and
    /// SetInterfaceDnsSettings on the adapter, whose metric is 5 (§5.3, §7). The bond's subnets are
    /// on the adapter already; the mesh's more specific routes keep winning.
    pub fn capture_install(c: &Capture) -> io::Result<()> {
        let luid = luid_of(&c.tun).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no wirebond adapter"))?;
        if c.routes == Routes::All {
            for net in halves() {
                route(&luid, net, true).or_else(|e| if net.0.is_ipv6() { ipv6_off(e) } else { Err(e) })?;
            }
        }
        if let Some(a) = c.dns {
            dns(guid_of(&luid)?, a.is_ipv6(), Some(a))?;
        }
        Ok(())
    }

    /// An IPv6 route fails where IPv6 is unbound from the adapter: IPv4 capture still stands.
    fn ipv6_off(e: io::Error) -> io::Result<()> {
        if e.raw_os_error() == Some(ERROR_NOT_FOUND as i32) { Ok(()) } else { Err(e) }
    }

    /// Deletes the four routes and the adapter's DNS server: only what capture_install made.
    pub fn capture_remove(c: &Capture) -> io::Result<()> {
        let Some(luid) = luid_of(&c.tun) else { return Ok(()) }; // the adapter, and all on it, is gone
        let mut r = Ok(());
        for net in halves() {
            r = r.and(route(&luid, net, false));
        }
        if let Some(a) = c.dns {
            r = r.and(guid_of(&luid).and_then(|g| dns(g, a.is_ipv6(), None)));
        }
        r
    }

    /// Internet Connection Sharing (HNetCfg.HNetShare) from the uplink to the adapter, which ICS
    /// readdresses to 192.168.137.1/24: `EgressOn::addr4` says so. IPv4 only; ICS busy or refused
    /// gives v4 false and local delivery only (§6.2).
    pub fn egress_enable(e: &Egress) -> io::Result<EgressOn> {
        let uplink = uplink();
        let v4 = match &uplink {
            Some(up) => ics(up, &e.tun, true).is_ok(),
            None => false,
        };
        Ok(EgressOn { v4, v6: false, addr4: ICS_ADDR, uplink })
    }

    /// Turns off the sharing whose private side is the adapter: only what egress_enable made.
    pub fn egress_disable(e: &Egress) -> io::Result<()> {
        ics("", &e.tun, false)
    }

    /// The friendly name of the default route's interface (§6.2).
    fn uplink() -> Option<String> {
        let index = best_index()?;
        super::ifaces().ok()?.into_iter().find(|i| i.index == index).map(|i| i.name)
    }

    /// The index of the interface that reaches the internet (1.1.1.1), never the adapter's own /1.
    fn best_index() -> Option<u32> {
        // SAFETY: SOCKADDR_IN is plain data.
        let mut sa: SOCKADDR_IN = unsafe { mem::zeroed() };
        sa.sin_family = AF_INET;
        sa.sin_addr.S_un.S_addr = u32::from_ne_bytes([1, 1, 1, 1]);
        let mut index = 0;
        // SAFETY: sa is a SOCKADDR_IN, read as a SOCKADDR, and index an out parameter.
        let r = unsafe { GetBestInterfaceEx((&raw const sa).cast(), &mut index) };
        (r == 0).then_some(index)
    }

    /// Shares `public` to `private` through HNetCfg.HNetShare, driven by PowerShell (§6.2, §8), or,
    /// with `enable` false, ends the sharing whose private side is `private`. ICS serves one shared
    /// pair at a time: a pair of ours moves to the new public side (the uplink changed); one in use by
    /// something else, Mobile Hotspot included, is an error and stays.
    fn ics(public: &str, private: &str, enable: bool) -> io::Result<()> {
        let q = |s: &str| s.replace('\'', "''");
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n\
             $m = New-Object -ComObject HNetCfg.HNetShare\n\
             $pub = $null; $priv = $null; $others = @()\n\
             foreach ($c in $m.EnumEveryConnection) {{\n\
               $n = $m.NetConnectionProps.Invoke($c).Name\n\
               $s = $m.INetSharingConfigurationForINetConnection.Invoke($c)\n\
               if ($n -eq '{pub}') {{ $pub = $s }} elseif ($n -eq '{priv}') {{ $priv = $s }}\n\
               elseif ($s.SharingEnabled) {{ $others += $s }}\n\
             }}\n\
             $ours = $priv -and $priv.SharingEnabled -and $priv.SharingConnectionType -eq 1\n\
             if ({enable}) {{\n\
               if (-not $pub -or -not $priv) {{ throw 'no connection named {pub} or {priv}' }}\n\
               if ($others -and -not $ours) {{ throw 'Internet Connection Sharing is in use by another connection' }}\n\
               foreach ($s in $others) {{ $s.DisableSharing() }}\n\
               if (-not ($pub.SharingEnabled -and $pub.SharingConnectionType -eq 0)) {{ $pub.EnableSharing(0) }}\n\
               if (-not $ours) {{ $priv.EnableSharing(1) }}\n\
             }} elseif ($ours) {{\n\
               $priv.DisableSharing()\n\
               foreach ($s in $others) {{ if ($s.SharingConnectionType -eq 0) {{ $s.DisableSharing() }} }}\n\
               if ($pub -and $pub.SharingEnabled) {{ $pub.DisableSharing() }}\n\
             }}\n",
            pub = q(public),
            priv = q(private),
            enable = if enable { "$true" } else { "$false" },
        );
        powershell(&script)
    }

    /// Runs a PowerShell script without a window or a profile; its error becomes ours.
    fn powershell(script: &str) -> io::Result<()> {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
        let out = c.creation_flags(CREATE_NO_WINDOW).output()?;
        let err = String::from_utf8_lossy(&out.stderr);
        if out.status.success() { Ok(()) } else { Err(io::Error::other(format!("powershell: {}", err.trim()))) }
    }

    /// The first DNS server of the default-route adapter, from GetAdaptersAddresses (§7), port 53.
    pub fn upstream_resolver() -> io::Result<SocketAddr> {
        let index = best_index().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no default route"))?;
        let (mut buf, mut len) = (Vec::<u64>::new(), 15_000u32);
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_UNICAST;
        let r = loop {
            buf.resize(len.div_ceil(8) as usize, 0);
            // SAFETY: buf holds len bytes, aligned for IP_ADAPTER_ADDRESSES_LH; len is in and out.
            match unsafe { GetAdaptersAddresses(AF_UNSPEC.into(), flags, null(), buf.as_mut_ptr().cast(), &mut len) } {
                ERROR_BUFFER_OVERFLOW => continue,
                r => break r,
            }
        };
        win32(r)?;
        let mut p = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: as in super::ifaces: the records and their DNS server lists point into buf.
        unsafe {
            while let Some(a) = p.as_ref() {
                if a.Anonymous1.Anonymous.IfIndex == index || a.Ipv6IfIndex == index {
                    let mut d = a.FirstDnsServerAddress;
                    while let Some(s) = d.as_ref() {
                        let sa = s.Address.lpSockaddr;
                        let ip = match (*sa).sa_family {
                            AF_INET => {
                                Some(IpAddr::from((*sa.cast::<SOCKADDR_IN>()).sin_addr.S_un.S_addr.to_ne_bytes()))
                            }
                            AF_INET6 => Some(IpAddr::from((*sa.cast::<SOCKADDR_IN6>()).sin6_addr.u.Byte)),
                            _ => None,
                        };
                        // A site-local fec0::/10 server is Windows' placeholder for none.
                        if let Some(ip) =
                            ip.filter(|ip| !matches!(ip, IpAddr::V6(a) if a.segments()[0] & 0xffc0 == 0xfec0))
                        {
                            return Ok(SocketAddr::new(ip, 53));
                        }
                        d = s.Next;
                    }
                }
                p = a.Next;
            }
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "the default route's adapter has no DNS server"))
    }

    /// ICS through HNetCfg.HNetShare, driven by PowerShell (§8). A refusal carries the manual steps.
    pub fn support_share(s: &Share) -> io::Result<()> {
        ics(&s.uplink, &s.cable, true).map_err(|e| {
            io::Error::other(format!(
                "{e}. By hand: Control Panel > Network Connections > {} > Properties > Sharing: allow other \
                 network users to connect through this computer's Internet connection, home networking \
                 connection {}",
                s.uplink, s.cable
            ))
        })
    }
    pub fn support_unshare(s: &Share) -> io::Result<()> {
        ics(&s.uplink, &s.cable, false)
    }

    /// Clears the DNS settings and ICS sharing a crash left on the adapter's GUID (§5.4). The adapter
    /// itself, its addresses and its routes go with the process that made it.
    pub fn cleanup_stale() -> io::Result<()> {
        for v6 in [false, true] {
            let _ = dns(ADAPTER_GUID, v6, None); // fails where the adapter never was
        }
        if luid_of(NAME).is_some() {
            let _ = ics("", NAME, false);
        }
        Ok(())
    }

    /// The daemon's wait (§4.3): every socket associated with one event through WSAEventSelect, and
    /// WaitForMultipleObjects on that event, Wintun's read-wait event and a high-resolution waitable
    /// timer. After a wait, the loop drains each source without blocking.
    pub struct EventLoop {
        /// A WSAEVENT, which is a HANDLE.
        event: WSAEVENT,
        timer: HANDLE,
    }

    // SAFETY: the two handles are kernel objects, usable from any thread.
    unsafe impl Send for EventLoop {}

    impl EventLoop {
        pub fn new() -> io::Result<EventLoop> {
            // SAFETY: WSACreateEvent takes nothing; a manual-reset event, reset in wait.
            let event = unsafe { WSACreateEvent() };
            if event == 0 {
                return Err(io::Error::last_os_error());
            }
            let access = TIMER_ALL_ACCESS;
            // SAFETY: no attributes and no name. The high-resolution timer needs Windows 10 1803; an
            // ordinary one is the fallback, which timeBeginPeriod(1) makes fine enough.
            let mut timer =
                unsafe { CreateWaitableTimerExW(null(), null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, access) };
            if timer.is_null() {
                // SAFETY: as above.
                timer = unsafe { CreateWaitableTimerExW(null(), null(), 0, access) };
            }
            if timer.is_null() {
                let e = io::Error::last_os_error();
                // SAFETY: event is open and closed only here.
                unsafe { WSACloseEvent(event) };
                return Err(e);
            }
            Ok(EventLoop { event, timer })
        }

        /// Associates `s` with the event: a datagram, a connection to accept, or data or a close on a
        /// stream signals it. It makes `s` nonblocking.
        pub fn add(&self, s: &impl AsRawSocket) -> io::Result<()> {
            let what = (FD_READ | FD_ACCEPT | FD_CLOSE) as i32;
            // SAFETY: s is an open socket and the event is open.
            if unsafe { WSAEventSelect(s.as_raw_socket() as SOCKET, self.event, what) } != 0 {
                // SAFETY: WSAGetLastError only reads this thread's last error.
                return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
            }
            Ok(())
        }

        /// Ends the association, as before closing `s` elsewhere; `s` stays nonblocking.
        pub fn remove(&self, s: &impl AsRawSocket) {
            // SAFETY: s is an open socket; no event and no events ends the association.
            unsafe { WSAEventSelect(s.as_raw_socket() as SOCKET, 0, 0) };
        }

        /// Associates node's waker as `add` does a socket, so a wake from another thread ends the
        /// wait (as `Poll::add_waker` does for a poll).
        pub fn add_waker(&self, w: &super::super::Waker) -> io::Result<()> {
            self.add(&w.rx)
        }

        /// Waits until a socket or the TUN device has something to read or `t` has passed, never less.
        /// On return it resets the sockets' event: the loop then drains every source, and whatever
        /// arrives meanwhile signals the next wait.
        pub fn wait(&self, tun: Option<HANDLE>, t: Duration) -> io::Result<()> {
            // Relative, in 100 ns units: negative.
            let due = -(i64::try_from(t.as_nanos().div_ceil(100)).unwrap_or(i64::MAX).max(1));
            // SAFETY: the timer is open and due outlives the call; no completion routine.
            if unsafe { SetWaitableTimer(self.timer, &due, 0, None, null(), 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut handles = vec![self.event as HANDLE, self.timer];
            handles.extend(tun.filter(|h| !h.is_null()));
            // A backstop in case the timer is late: the timeout rounded up, plus a millisecond.
            let ms = u32::try_from(t.as_millis().saturating_add(1)).unwrap_or(INFINITE - 1);
            // SAFETY: handles holds open handles, at most three.
            let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, ms) };
            // SAFETY: the event is open.
            unsafe { WSAResetEvent(self.event) };
            if r == WAIT_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
        }
    }

    impl Drop for EventLoop {
        fn drop(&mut self) {
            // SAFETY: both handles are open, and only this closes them.
            unsafe {
                WSACloseEvent(self.event);
                CloseHandle(self.timer);
            }
        }
    }
}
