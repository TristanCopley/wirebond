//! The CLI against a fake host and a fake daemon: nothing is installed or dialled for real. The
//! views are golden files: `testdata/<name>.json` (a Status or a Pick) renders to
//! `testdata/<name>.txt`; WIREBOND_BLESS=1 cargo test -p wirebond cli rewrites the .txt files.

use super::*;
use crate::api::{ApiError, Pick, Status};
use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::sync::{Arc, Mutex};

type Daemon = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// The fake machine: whether a daemon answers, who we are, and what was asked of it.
struct Fake {
    admin: bool,
    running: Cell<bool>,
    /// Whether install starts a daemon.
    starts: bool,
    install_err: Option<&'static str>,
    installs: Cell<u32>,
    uninstalls: Cell<u32>,
    on_path: bool,
    current: PathBuf,
    windows: bool,
    daemon: Daemon,
    asked: Arc<Mutex<Vec<Request>>>,
    socks: RefCell<Vec<String>>,
}

impl Fake {
    fn new(daemon: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Fake {
        Fake {
            admin: false,
            running: Cell::new(true),
            starts: true,
            install_err: None,
            installs: Cell::new(0),
            uninstalls: Cell::new(0),
            on_path: true,
            current: "/usr/local/bin/wirebond".into(),
            windows: false,
            daemon: Arc::new(daemon),
            asked: Arc::default(),
            socks: RefCell::default(),
        }
    }

    fn run(&self, line: &str) -> (i32, String, String) {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        cli(&args, "2.0.0", "abc123", self)
    }

    fn asked(&self) -> Vec<Request> {
        self.asked.lock().unwrap().clone()
    }
}

impl Host for Fake {
    fn scope(&self) -> Scope {
        Scope::System
    }
    fn admin(&self) -> bool {
        self.admin
    }
    fn dial(&self, _: Scope, sock: &str) -> io::Result<Box<dyn LocalStream>> {
        self.socks.borrow_mut().push(sock.into());
        if !self.running.get() {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        Ok(Box::new(Conn { wrote: Vec::new(), back: None, daemon: self.daemon.clone(), asked: self.asked.clone() }))
    }
    fn install(&self, _: Scope) -> io::Result<bool> {
        self.installs.set(self.installs.get() + 1);
        if let Some(e) = self.install_err {
            return Err(io::Error::other(e));
        }
        self.running.set(self.starts);
        Ok(true)
    }
    fn uninstall(&self, _: Scope) -> io::Result<(bool, String)> {
        self.uninstalls.set(self.uninstalls.get() + 1);
        Ok((true, String::new()))
    }
    fn on_path(&self, _: Scope) -> bool {
        self.on_path
    }
    fn exe(&self, _: Scope) -> PathBuf {
        "/usr/local/bin/wirebond".into()
    }
    fn current_exe(&self) -> PathBuf {
        self.current.clone()
    }
    fn windows(&self) -> bool {
        self.windows
    }
    fn start_wait(&self) -> Duration {
        Duration::from_millis(100)
    }
}

/// One connection to the fake daemon: the request line it is written, then its reply line.
struct Conn {
    wrote: Vec<u8>,
    back: Option<Cursor<Vec<u8>>>,
    daemon: Daemon,
    asked: Arc<Mutex<Vec<Request>>>,
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.wrote.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if self.back.is_none() {
            assert_eq!(self.wrote.last(), Some(&b'\n'), "one request line");
            let req: Request = serde_json::from_slice(&self.wrote).expect("a request line");
            self.asked.lock().unwrap().push(req.clone());
            let mut line = serde_json::to_vec(&(self.daemon)(&req)).unwrap();
            line.push(b'\n');
            self.back = Some(Cursor::new(line));
        }
        self.back.as_mut().unwrap().read(b)
    }
}

impl LocalStream for Conn {
    fn set_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
}

fn p(s: &str) -> Result<(Cmd, bool), String> {
    parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
}

fn api(s: &str) -> Request {
    match p(s) {
        Ok((Cmd::Api(r), _)) => r,
        other => panic!("{s}: {other:?}"),
    }
}

/// A test data file.
fn data(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/testdata").join(name);
    fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n")
}

fn status(name: &str) -> Status {
    serde_json::from_str(&data(&format!("{name}.json"))).unwrap_or_else(|e| panic!("{name}.json: {e}"))
}

/// `got` must be `testdata/<name>.txt`, or with WIREBOND_BLESS set becomes it.
fn golden(name: &str, got: &str) {
    if env::var_os("WIREBOND_BLESS").is_some() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/testdata").join(format!("{name}.txt"));
        fs::write(path, got).unwrap();
        return;
    }
    assert_eq!(got, data(&format!("{name}.txt")), "{name}: WIREBOND_BLESS=1 cargo test -p wirebond cli accepts it");
}

#[test]
fn every_verb_and_flag_of_section_19_parses() {
    assert_eq!(api(""), Request::Status { device: None });
    assert_eq!(api("status"), Request::Status { device: None });
    assert_eq!(p("--json").unwrap(), (Cmd::Api(Request::Status { device: None }), true));
    assert_eq!(api("status xanth --json"), Request::Status { device: Some("xanth".into()) });
    assert_eq!(api("--json status xanth"), Request::Status { device: Some("xanth".into()) });
    assert!(p("status --json").unwrap().1);
    assert!(!p("status").unwrap().1);
    assert_eq!(api("client xanth"), Request::Client { anchor: "xanth".into(), mode: None });
    assert_eq!(
        api("client xanth performance"),
        Request::Client { anchor: "xanth".into(), mode: Some(Mode::Performance) }
    );
    assert_eq!(api("client xanth bandwidth"), Request::Client { anchor: "xanth".into(), mode: Some(Mode::Bandwidth) });
    assert_eq!(api("client 100.64.0.7"), Request::Client { anchor: "100.64.0.7".into(), mode: None });
    assert_eq!(api("client off"), Request::ClientOff);
    assert_eq!(api("mode performance"), Request::Mode { mode: Mode::Performance });
    assert_eq!(api("mode bandwidth"), Request::Mode { mode: Mode::Bandwidth });
    let pick = Request::Pick {
        devices: vec!["a".into(), "b".into()],
        mode: Some(Mode::Bandwidth),
        seconds: Some(5),
        dry_run: true,
    };
    assert_eq!(api("pick a --mode bandwidth b --seconds 5 --dry-run"), pick);
    assert_eq!(api("pick a b --mode=bandwidth --seconds=5 --dry-run"), pick);
    assert_eq!(api("pick"), Request::Pick { devices: vec![], mode: None, seconds: None, dry_run: false });
    assert_eq!(api("pick --add vps"), Request::PickAdd { device: "vps".into() });
    assert_eq!(api("pick --remove vps"), Request::PickRemove { device: "vps".into() });
    assert_eq!(api("anchor on"), Request::AnchorOn);
    assert_eq!(api("anchor off"), Request::AnchorOff);
    assert_eq!(api("anchor allow mac pc"), Request::AnchorAllow { devices: vec!["mac".into(), "pc".into()] });
    assert_eq!(api("anchor deny pc"), Request::AnchorDeny { devices: vec!["pc".into()] });
    let on = Request::SupportOn { uplink: "eth0".into(), cable: "usb0".into(), for_client: Some("mac".into()) };
    assert_eq!(api("support on eth0 usb0 --for mac"), on);
    assert_eq!(
        api("support on eth0 usb0"),
        Request::SupportOn { uplink: "eth0".into(), cable: "usb0".into(), for_client: None }
    );
    assert_eq!(api("support off"), Request::SupportOff);
    assert_eq!(api("update"), Request::Update);
    assert_eq!(p("uninstall").unwrap().0, Cmd::Uninstall);
    assert_eq!(p("version").unwrap(), (Cmd::Version, false));
    assert_eq!(p("version --json").unwrap(), (Cmd::Version, true));
    assert_eq!(p("--version").unwrap(), (Cmd::Version, false));
    assert_eq!(p("run").unwrap().0, Cmd::Run);
    assert_eq!(p("help").unwrap().0, Cmd::Help);
    assert_eq!(p("client xanth --help").unwrap().0, Cmd::Help);
}

#[test]
fn usage_errors() {
    for bad in [
        "client xanth stable",
        "client xanth combine",
        "mode capacity",
        "mode",
        "mode performance bandwidth",
        "client",
        "client a b c",
        "client off performance",
        "anchor",
        "anchor on now",
        "anchor allow",
        "anchor deny",
        "support on eth0",
        "support on eth0 usb0 extra",
        "support off now",
        "support",
        "status a b",
        "update now",
        "uninstall all",
        "version 2",
        "connect x",
        "serve 47984",
        "pick --add",
        "pick --add a --remove b",
        "pick --add a b",
        "pick --add a --dry-run",
        "pick --remove a --mode performance",
        "pick a --seconds 0",
        "pick a --seconds x",
        "pick a --mode stable",
        "client xanth --dry-run",
        "client xanth --seconds 3",
        "mode bandwidth --mode bandwidth",
        "anchor on --for mac",
        "status --add x",
        "uninstall --dry-run",
        "version --mode bandwidth",
        "status --frobnicate",
    ] {
        assert!(p(bad).is_err(), "{bad}");
        let (code, out, err) = Fake::new(|_| Reply::ok("")).run(bad);
        assert_eq!((code, out.as_str()), (USAGE, ""), "{bad}");
        assert!(err.starts_with("wirebond: ") && err.contains("wirebond status [<device>] [--json]"), "{bad}: {err}");
    }
    assert!(p("client ''").is_ok(), "a word, though an odd one");
    assert!(parse(&["client".into(), String::new()]).is_err(), "an empty device");
    // Under --json the usage error is a reply line too.
    let (code, out, err) = Fake::new(|_| Reply::ok("")).run("mode combine --json");
    assert_eq!((code, err.as_str()), (USAGE, ""));
    let r: Reply = serde_json::from_str(&out).unwrap();
    assert_eq!((r.ok, r.error.unwrap().code.as_str()), (false, "usage"));
}

#[test]
fn exit_codes_follow_the_error() {
    let fail = |c: &str| exit_code(&Reply::fail(c, ""));
    assert_eq!((fail("not_allowed"), fail("too_many_bonds"), fail("version")), (REFUSED, REFUSED, REFUSED));
    assert_eq!((fail("admin"), fail("no_daemon")), (NEEDS_ADMIN, NO_DAEMON));
    assert_eq!((fail("usage"), fail("bad_request")), (USAGE, USAGE));
    assert_eq!((fail("failed"), fail("busy"), fail("unknown"), fail("")), (FAILED, FAILED, FAILED, FAILED));
    assert_eq!(exit_code(&Reply::ok("")), OK);
    assert_eq!(exit_code(&Reply::default()), FAILED, "not ok, though no error says why");
    // End to end, through the fake daemon: the code and where the reason goes.
    for (code, want) in [
        ("not_allowed", REFUSED),
        ("too_many_bonds", REFUSED),
        ("version", REFUSED),
        ("busy", FAILED),
        ("failed", FAILED),
        ("bad_request", USAGE),
    ] {
        let f = Fake::new(move |_| Reply::fail(code, format!("{code} from the anchor")));
        let (got, out, err) = f.run("client xanth");
        assert_eq!((got, out.as_str()), (want, ""), "{code}");
        assert_eq!(err, format!("wirebond: {code} from the anchor\n"));
        let (got, out, _) = f.run("client xanth --json");
        assert_eq!(got, want);
        let r: Reply = serde_json::from_str(&out).unwrap();
        assert_eq!(r.error, Some(ApiError { code: code.into(), msg: format!("{code} from the anchor") }));
    }
    // A daemon that fails without saying why.
    let (code, _, err) = Fake::new(|_| Reply::default()).run("anchor on");
    assert_eq!((code, err.as_str()), (FAILED, "wirebond: the daemon failed without saying why\n"));
}

#[test]
fn version_answers_without_a_daemon() {
    let f = Fake::new(|_| unreachable!("version never asks the daemon"));
    f.running.set(false);
    assert_eq!(f.run("version"), (OK, "wirebond 2.0.0\n".into(), String::new()));
    // What the update's smoke test reads (§22.3).
    assert_eq!(f.run("version --json"), (OK, "{\"version\":\"2.0.0\",\"commit\":\"abc123\"}\n".into(), String::new()));
    assert_eq!(f.run("--version --json").1, "{\"version\":\"2.0.0\",\"commit\":\"abc123\"}\n");
    let (code, out, _) = f.run("help");
    assert_eq!((code, out.as_str()), (OK, HELP));
    assert!(f.socks.borrow().is_empty());
}

#[test]
fn status_needs_a_daemon_and_never_installs() {
    let mut f = Fake::new(|_| unreachable!());
    f.admin = true;
    f.running.set(false);
    let (code, out, err) = f.run("status");
    assert_eq!((code, out.as_str()), (NO_DAEMON, ""));
    assert!(err.contains("no wirebond daemon answers here"), "{err}");
    let (code, out, _) = f.run("status xanth --json");
    assert_eq!(code, NO_DAEMON);
    assert_eq!(serde_json::from_str::<Reply>(&out).unwrap().error.unwrap().code, "no_daemon");
    assert_eq!(f.installs.get(), 0);
    assert!(f.socks.borrow().iter().all(|s| s == host::STATUS_SOCK), "status asks on status.sock");
}

#[test]
fn the_first_changing_verb_installs_once_as_an_administrator() {
    // Not an administrator: nothing is installed, and the exact command to run is printed.
    let mut f = Fake::new(|r| match r {
        Request::Client { .. } => Reply { changed: true, ..Reply::ok("") },
        _ => Reply::ok(""),
    });
    f.running.set(false);
    f.current = "/home/me/Downloads/wirebond".into();
    let (code, out, err) = f.run("client xanth bandwidth");
    assert_eq!((code, out.as_str()), (NEEDS_ADMIN, ""));
    assert_eq!(
        err,
        "wirebond: wirebond is not running here, and installing it needs sudo once\n  run: sudo '/home/me/Downloads/wirebond' client xanth bandwidth\n"
    );
    assert_eq!(f.installs.get(), 0);
    // As an administrator: installed, started, asked; the reply says so.
    f.admin = true;
    let (code, out, err) = f.run("client xanth bandwidth");
    assert_eq!((code, err.as_str()), (OK, ""));
    assert_eq!(
        out,
        "this device is a client of xanth in bandwidth mode; wirebond status shows the bond\n\
         wirebond is installed as a system service at /usr/local/bin/wirebond\n"
    );
    assert_eq!(f.installs.get(), 1);
    assert_eq!(f.asked(), vec![Request::Client { anchor: "xanth".into(), mode: Some(Mode::Bandwidth) }]);
    // From the installed executable with the daemon answering, nothing is installed again.
    f.current = "/usr/local/bin/wirebond".into();
    let (code, out, _) = f.run("anchor allow mac");
    assert_eq!((code, out.as_str()), (OK, "allowed: mac\n(no change)\n"));
    assert_eq!(f.installs.get(), 1);
    assert!(f.socks.borrow().iter().all(|s| s == host::CTL_SOCK), "changes go to ctl.sock");
    // A new download run as an administrator mends the install (1.x).
    f.current = "/tmp/new/wirebond".into();
    assert_eq!(f.run("anchor on").0, OK);
    assert_eq!(f.installs.get(), 2);
    // Not an administrator, with the daemon answering: asked directly.
    f.admin = false;
    assert_eq!(f.run("mode performance").0, OK);
    assert_eq!(f.installs.get(), 2);
    // On Windows the command is for an administrator PowerShell.
    let mut w = Fake::new(|_| Reply::ok(""));
    (w.windows, w.current) = (true, r"C:\Users\me\Downloads\wirebond.exe".into());
    w.running.set(false);
    let (code, _, err) = w.run("anchor on");
    assert_eq!(code, NEEDS_ADMIN);
    assert!(err.ends_with("run: & 'C:\\Users\\me\\Downloads\\wirebond.exe' anchor on\n"), "{err}");
    assert!(err.contains("an administrator PowerShell"), "{err}");
}

#[test]
fn a_failed_install_and_a_silent_daemon() {
    let mut f = Fake::new(|_| Reply::ok(""));
    (f.admin, f.install_err) = (true, Some("systemctl: not found"));
    f.running.set(false);
    let (code, _, err) = f.run("anchor on");
    assert_eq!((code, err.as_str()), (FAILED, "wirebond: installing wirebond failed: systemctl: not found\n"));
    let mut f = Fake::new(|_| Reply::ok(""));
    (f.admin, f.starts) = (true, false);
    f.running.set(false);
    let (code, _, err) = f.run("anchor on");
    assert_eq!(code, NO_DAEMON);
    assert!(err.starts_with("wirebond: wirebond is installed but its daemon does not answer"), "{err}");
    // Installed off PATH: where it is now.
    let mut f = Fake::new(|_| Reply { changed: true, ..Reply::ok("") });
    (f.admin, f.on_path) = (true, false);
    f.running.set(false);
    let (code, out, _) = f.run("anchor on");
    assert_eq!(code, OK);
    assert!(out.ends_with("; it is not on PATH: run it as '/usr/local/bin/wirebond'\n"), "{out}");
}

#[test]
fn uninstall_needs_an_administrator() {
    let mut f = Fake::new(|_| unreachable!());
    let (code, _, err) = f.run("uninstall");
    assert_eq!(code, NEEDS_ADMIN);
    assert!(err.contains("run: sudo '/usr/local/bin/wirebond' uninstall"), "{err}");
    f.admin = true;
    assert_eq!(f.run("uninstall"), (OK, "wirebond is uninstalled\n".into(), String::new()));
    let (code, out, _) = f.run("uninstall --json");
    assert_eq!((code, out.as_str()), (OK, "{\"ok\":true,\"changed\":true,\"message\":\"wirebond is uninstalled\"}\n"));
    assert_eq!(f.uninstalls.get(), 2);
}

#[test]
fn confirmations() {
    let f = Fake::new(|r| match r {
        Request::Update => Reply { changed: true, ..Reply::ok("updating to 2.0.1 once idle") },
        Request::ClientOff => Reply::ok(""),
        _ => Reply { changed: true, ..Reply::ok("") },
    });
    let cases = [
        ("client xanth", "this device is a client of xanth; wirebond status shows the bond\n"),
        ("client off", "the bond is off; traffic goes direct\n(no change)\n"),
        ("mode bandwidth", "the bond's mode is bandwidth\n"),
        ("pick --add vps", "vps is a pick candidate\n"),
        ("pick --remove vps", "vps is not a pick candidate\n"),
        ("anchor on", "this device accepts bonds from the devices it allows (wirebond anchor allow <device>)\n"),
        ("anchor off", "this device accepts no bonds\n"),
        ("anchor allow mac pc", "allowed: mac, pc\n"),
        ("anchor deny pc", "denied: pc\n"),
        ("support on eth0 usb0 --for mac", "sharing eth0 to usb0 for mac\n"),
        ("support off", "sharing is off\n"),
        ("update", "updating to 2.0.1 once idle\n"),
    ];
    for (line, want) in cases {
        assert_eq!(f.run(line), (OK, want.into(), String::new()), "{line}");
    }
}

#[test]
fn status_json_is_section_20_2_and_the_rest_is_the_reply_line() {
    let st = status("client");
    let reply = Reply { ok: true, status: Some(Box::new(st.clone())), ..Reply::default() };
    let f = Fake::new(move |_| reply.clone());
    let (code, out, _) = f.run("status --json");
    assert_eq!(code, OK);
    assert_eq!(out, serde_json::to_string(&st).unwrap() + "\n", "the status itself, not the reply around it");
    assert!(out.starts_with("{\"device\":\"mac\",\"role\":\"client\",\"version\":\"2.0.0\",\"links\":["), "{out}");
    assert_eq!(serde_json::from_str::<Status>(&out).unwrap(), st);
    // pick --json is the reply line, with the pick in it.
    let pick: Pick = serde_json::from_str(&data("pick-performance.json")).unwrap();
    let reply = Reply { ok: true, changed: true, pick: Some(pick.clone()), ..Reply::default() };
    let f = Fake::new(move |_| reply.clone());
    let (code, out, _) = f.run("pick xanth vps --json");
    assert_eq!(code, OK);
    assert_eq!(serde_json::from_str::<Reply>(&out).unwrap().pick, Some(pick));
}

#[test]
fn json_escapes_what_a_terminal_would_act_on() {
    let st = Status { device: "evil\u{1b}[2J\u{9b}31m\u{202e}".into(), ..Status::default() };
    let line = render::json_line(&st);
    assert!(line.starts_with(r#"{"device":"evil\u001b[2J\u009b31m\u202e","#), "{line}");
    assert!(!line.trim_end().contains(render::hidden));
    assert_eq!(serde_json::from_str::<Status>(&line).unwrap(), st, "the same string, read back");
}

#[test]
fn bytes_for_people() {
    let b = render::bytes;
    assert_eq!(
        [b(0), b(999), b(1000), b(1_234_567), b(310_000_000), b(9_990_000_000), b(12_000_000_000_000)],
        ["0 B", "999 B", "1.0 kB", "1.2 MB", "310 MB", "10.0 GB", "12.0 TB"]
    );
}

/// Every view, from its fixture, through the CLI as a person sees it.
#[test]
fn golden_views() {
    for (name, remote) in [
        ("client", None),
        ("client-bandwidth", None),
        ("client-connecting", None),
        ("anchor", None),
        ("support", None),
        ("none", None),
        ("remote", Some("xanth")),
        ("hostile", None),
    ] {
        let st = status(name);
        let reply = Reply { ok: true, status: Some(Box::new(st)), ..Reply::default() };
        let f = Fake::new(move |_| reply.clone());
        let line = match remote {
            Some(d) => format!("status {d}"),
            None => "status".into(),
        };
        let (code, out, err) = f.run(&line);
        assert_eq!((code, err.as_str()), (OK, ""), "{name}");
        assert_eq!(f.asked(), vec![Request::Status { device: remote.map(String::from) }]);
        golden(name, &out);
        for l in out.lines() {
            assert!(l.chars().count() <= 100, "{name}: wider than 100 columns: {l}");
            assert!(!l.contains(render::hidden), "{name}: a hidden character: {l:?}");
        }
    }
    for name in ["pick-performance", "pick-bandwidth"] {
        let pick: Pick = serde_json::from_str(&data(&format!("{name}.json"))).unwrap();
        let reply = Reply { ok: true, pick: Some(pick), ..Reply::default() };
        let f = Fake::new(move |_| reply.clone());
        let (code, out, _) = f.run("pick");
        assert_eq!(code, OK);
        golden(name, &out);
    }
}
