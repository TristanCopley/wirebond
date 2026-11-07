//! The CLI (SPEC 2.0 §19): the verbs parsed into the local API's [`Request`] (§20.1), sent to the
//! daemon, and the [`Reply`] printed for people (render.rs) or as JSON: `status --json` prints the
//! §20.2 status itself, any other verb the reply line. `run` is the daemon itself; `version` and
//! `uninstall` never reach it. The first changing verb installs and starts the service, as 1.x
//! did. Owner: cli and api rendering.

mod render;
#[cfg(test)]
mod tests;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use wirebond_core::MAX_MSG;

use crate::api::{Mode, Reply, Request, Version};
use crate::host::{self, LocalStream, Scope};
use render::{json_line, pick_text, status_text};

/// Exit codes (§19).
pub const OK: i32 = 0;
pub const FAILED: i32 = 1;
pub const USAGE: i32 = 2;
pub const NO_DAEMON: i32 = 3;
pub const NEEDS_ADMIN: i32 = 4;
pub const REFUSED: i32 = 5;

const HELP: &str = "\
wirebond                                   same as: wirebond status
wirebond status [<device>] [--json]        this node, or another node's read-only view over the mesh
wirebond client <anchor> [performance|bandwidth]
                                           make this node a client of <anchor> and start its bond
wirebond client off                        stop the bond; traffic goes direct
wirebond mode performance|bandwidth        change the bond's mode
wirebond pick [<device>...] [--mode M] [--seconds N] [--dry-run]
wirebond pick --add <device> | --remove <device>
wirebond anchor on|off                     accept bonds on this node
wirebond anchor allow <device>... | deny <device>...
wirebond support on <uplink> <cable> [--for <client>]
wirebond support off
wirebond update                            update now
wirebond uninstall                         remove everything wirebond created
wirebond version [--json]
wirebond run                               the daemon (started by the service manager)
";

/// How long a service the CLI just installed has to answer.
const START_WAIT: Duration = Duration::from_secs(5);

/// What a command line asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// A verb the daemon answers.
    Api(Request),
    Version,
    Uninstall,
    Run,
    Help,
}

/// What the CLI needs of this machine: host's install functions and local sockets, which the
/// tests fake.
trait Host {
    /// The scope the daemon runs in (host::need).
    fn scope(&self) -> Scope;
    /// Whether this process may install the system service.
    fn admin(&self) -> bool;
    fn dial(&self, s: Scope, sock: &str) -> io::Result<Box<dyn LocalStream>>;
    /// Installs and starts the service; whether anything changed.
    fn install(&self, s: Scope) -> io::Result<bool>;
    fn uninstall(&self, s: Scope) -> io::Result<(bool, String)>;
    fn on_path(&self, s: Scope) -> bool;
    /// The scope's installed executable, and the one running.
    fn exe(&self, s: Scope) -> PathBuf;
    fn current_exe(&self) -> PathBuf;
    fn windows(&self) -> bool;
    /// How long a started daemon has to answer.
    fn start_wait(&self) -> Duration;
}

/// This machine.
struct System;

impl Host for System {
    fn scope(&self) -> Scope {
        host::need(false)
    }
    fn admin(&self) -> bool {
        host::admin()
    }
    fn dial(&self, s: Scope, sock: &str) -> io::Result<Box<dyn LocalStream>> {
        host::dial(s, sock)
    }
    fn install(&self, s: Scope) -> io::Result<bool> {
        host::install(s)
    }
    fn uninstall(&self, s: Scope) -> io::Result<(bool, String)> {
        host::uninstall(s)
    }
    fn on_path(&self, s: Scope) -> bool {
        host::on_path(s)
    }
    fn exe(&self, s: Scope) -> PathBuf {
        s.paths().0
    }
    fn current_exe(&self) -> PathBuf {
        env::current_exe().unwrap_or_default()
    }
    fn windows(&self) -> bool {
        cfg!(windows)
    }
    fn start_wait(&self) -> Duration {
        START_WAIT
    }
}

/// Runs the CLI with the process's arguments and returns the exit code.
pub fn main(version: &str, commit: &str) -> i32 {
    let args: Vec<String> = env::args().skip(1).collect();
    let (code, out, err) = cli(&args, version, commit, &System);
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stderr().write_all(err.as_bytes());
    code
}

/// One command line: its exit code, stdout and stderr.
fn cli(args: &[String], version: &str, commit: &str, h: &dyn Host) -> (i32, String, String) {
    let (cmd, json) = match parse(args) {
        Ok(c) => c,
        Err(why) if args.iter().any(|a| a == "--json") => {
            return (USAGE, json_line(&Reply::fail("usage", why)), String::new());
        }
        Err(why) => return (USAGE, String::new(), format!("wirebond: {why}\n\n{HELP}")),
    };
    let req = match cmd {
        Cmd::Help => return (OK, HELP.into(), String::new()),
        Cmd::Version if json => {
            return (OK, json_line(&Version { version: version.into(), commit: commit.into() }), String::new());
        }
        Cmd::Version => return (OK, format!("wirebond {version}\n"), String::new()),
        Cmd::Run => {
            return match host::run_service(|stop| crate::node::run(version, stop)) {
                Ok(()) => (OK, String::new(), String::new()),
                Err(e) => (FAILED, String::new(), format!("wirebond: {e}\n")),
            };
        }
        Cmd::Uninstall => return output(None, &uninstall(args, h), json),
        Cmd::Api(req) => req,
    };
    let reply = ask(&req, args, h);
    output(Some(&req), &reply, json)
}

/// Prints a reply: as JSON (the §20.2 status for status, else the reply line), or for people,
/// with a failure on stderr.
fn output(req: Option<&Request>, r: &Reply, json: bool) -> (i32, String, String) {
    let code = exit_code(r);
    let status = matches!(req, Some(Request::Status { .. }));
    if json {
        return match &r.status {
            Some(st) if status && r.ok => (code, json_line(st), String::new()),
            _ => (code, json_line(r), String::new()),
        };
    }
    if let Some(e) = &r.error {
        let e = render::safe(e);
        let mut err = format!("wirebond: {}\n", if e.msg.is_empty() { &e.code } else { &e.msg });
        if !r.message.is_empty() {
            err += &format!("  {}\n", render::safe(&r.message));
        }
        return (code, String::new(), err);
    }
    let out = match (req, &r.status, &r.pick) {
        (Some(Request::Status { device }), Some(st), _) => status_text(st, device.as_deref()),
        (Some(Request::Status { .. }), None, _) => {
            return (FAILED, String::new(), "wirebond: no status came back\n".into());
        }
        (_, _, Some(p)) => pick_text(p) + &note(&r.message),
        (Some(req), _, _) => confirm(req, r),
        (None, _, _) => note(&r.message),
    };
    (code, out, String::new())
}

/// The daemon's message, one line, or nothing.
fn note(m: &str) -> String {
    if m.is_empty() { String::new() } else { render::safe(&m.to_string()) + "\n" }
}

/// Reads a command line (§19) into a command and whether `--json` was given. Flags may come
/// anywhere, as `--flag value` or `--flag=value`; a bare `wirebond` is status.
pub fn parse(args: &[String]) -> Result<(Cmd, bool), String> {
    let (mut json, mut dry_run) = (false, false);
    let (mut mode, mut seconds, mut for_client, mut add, mut remove) = (None, None, None, None, None);
    let mut words: Vec<&str> = Vec::new();
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a, None),
        };
        let mut value = |f: &str| match inline.clone().or_else(|| it.next().map(String::from)) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(format!("{f} needs a value")),
        };
        match flag {
            "--json" => json = true,
            "--dry-run" => dry_run = true,
            "--help" | "-h" => return Ok((Cmd::Help, json)),
            "--version" => return Ok((Cmd::Version, args.iter().any(|a| a == "--json"))),
            "--mode" => mode = Some(parse_mode(&value(flag)?)?),
            "--seconds" => {
                let v = value(flag)?;
                seconds =
                    Some(v.parse::<u32>().ok().filter(|&s| s > 0).ok_or(format!("--seconds {v:?}: not a count"))?);
            }
            "--for" => for_client = Some(value(flag)?),
            "--add" => add = Some(value(flag)?),
            "--remove" => remove = Some(value(flag)?),
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown flag {f}")),
            "" => return Err("an empty word: name a device".into()),
            w => words.push(w),
        }
    }
    let usage = |v: &str| Err(format!("usage: {}", form(v)));
    let owned = |ws: &[&str]| ws.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    // uninstall, version and run take no flag but --json.
    let flags = mode.is_some() || seconds.is_some() || for_client.is_some() || add.is_some() || remove.is_some();
    let bare =
        |cmd: Cmd| if flags || dry_run { Err("this command takes no flag but --json".into()) } else { Ok((cmd, json)) };
    let req = match words.as_slice() {
        [] | ["status"] => Request::Status { device: None },
        ["status", d] => Request::Status { device: Some(d.to_string()) },
        ["client", "off"] => Request::ClientOff,
        ["client", "off", ..] => return usage("client"),
        ["client", a] => Request::Client { anchor: a.to_string(), mode: None },
        ["client", a, m] => Request::Client { anchor: a.to_string(), mode: Some(parse_mode(m)?) },
        ["mode", m] => Request::Mode { mode: parse_mode(m)? },
        ["pick", devices @ ..] => match (add.take(), remove.take()) {
            (Some(d), None) if devices.is_empty() => Request::PickAdd { device: d },
            (None, Some(d)) if devices.is_empty() => Request::PickRemove { device: d },
            (None, None) => {
                Request::Pick { devices: owned(devices), mode: mode.take(), seconds: seconds.take(), dry_run }
            }
            _ => return usage("pick"),
        },
        ["anchor", "on"] => Request::AnchorOn,
        ["anchor", "off"] => Request::AnchorOff,
        ["anchor", "allow", ds @ ..] if !ds.is_empty() => Request::AnchorAllow { devices: owned(ds) },
        ["anchor", "deny", ds @ ..] if !ds.is_empty() => Request::AnchorDeny { devices: owned(ds) },
        ["support", "on", uplink, cable] => {
            Request::SupportOn { uplink: uplink.to_string(), cable: cable.to_string(), for_client: for_client.take() }
        }
        ["support", "off"] => Request::SupportOff,
        ["update"] => Request::Update,
        ["uninstall"] => return bare(Cmd::Uninstall),
        ["version"] => return bare(Cmd::Version),
        ["run"] => return bare(Cmd::Run),
        ["help"] => return Ok((Cmd::Help, json)),
        [v, ..] if !form(v).is_empty() => return usage(v),
        [w, ..] => return Err(format!("unknown command {w:?}: wirebond help lists them")),
    };
    // Flags that belong to another verb are usage errors.
    if mode.is_some() || seconds.is_some() || dry_run && !matches!(req, Request::Pick { .. }) {
        return Err("--mode, --seconds and --dry-run belong to pick".into());
    }
    if add.is_some() || remove.is_some() {
        return Err("--add and --remove belong to pick".into());
    }
    if for_client.is_some() {
        return Err("--for belongs to support on".into());
    }
    Ok((Cmd::Api(req), json))
}

/// A verb's forms, as `wirebond help` gives them; "" for no verb.
fn form(verb: &str) -> &'static str {
    match verb {
        "status" => "wirebond status [<device>] [--json]",
        "client" => "wirebond client <anchor> [performance|bandwidth] | wirebond client off",
        "mode" => "wirebond mode performance|bandwidth",
        "pick" => {
            "wirebond pick [<device>...] [--mode M] [--seconds N] [--dry-run] | pick --add <device> | pick --remove <device>"
        }
        "anchor" => "wirebond anchor on|off | anchor allow <device>... | anchor deny <device>...",
        "support" => "wirebond support on <uplink> <cable> [--for <client>] | wirebond support off",
        "update" => "wirebond update",
        "uninstall" => "wirebond uninstall",
        "version" => "wirebond version [--json]",
        "run" => "wirebond run",
        "help" => "wirebond help",
        _ => "",
    }
}

/// "performance" or "bandwidth"; nothing else (§14).
fn parse_mode(m: &str) -> Result<Mode, String> {
    Mode::parse(m).ok_or_else(|| format!("mode {m:?}: performance or bandwidth"))
}

/// The exit code of a reply (§19).
fn exit_code(r: &Reply) -> i32 {
    match r.error.as_ref().map(|e| e.code.as_str()) {
        None if r.ok => OK,
        Some("not_allowed" | "too_many_bonds" | "version") => REFUSED,
        Some("admin") => NEEDS_ADMIN,
        Some("no_daemon") => NO_DAEMON,
        Some("usage" | "bad_request") => USAGE,
        _ => FAILED,
    }
}

/// Asks the daemon (§20.1). A changing verb installs and starts the service first when no
/// daemon answers, or when this is not the installed executable (a new download), as 1.x did:
/// install is idempotent, so the first changing verb installs and later ones mend. Installing
/// needs administrator rights once; without them the reply says the exact command to run.
fn ask(req: &Request, args: &[String], h: &dyn Host) -> Reply {
    let s = h.scope();
    if !req.changes() {
        return match h.dial(s, host::STATUS_SOCK) {
            Ok(c) => exchange(c, req),
            Err(_) => Reply::fail(
                "no_daemon",
                "no wirebond daemon answers here; wirebond client <anchor> or wirebond anchor on, as an administrator, installs and starts it",
            ),
        };
    }
    let exe = h.exe(s);
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.into());
    let download = canon(&h.current_exe()) != canon(&exe);
    if let Ok(c) = h.dial(s, host::CTL_SOCK)
        && !(download && h.admin())
    {
        return exchange(c, req);
    }
    if !h.admin() {
        let how = if h.windows() { "an administrator PowerShell" } else { "sudo" };
        return Reply {
            message: format!("run: {}", again(h, &h.current_exe(), args)),
            ..Reply::fail("admin", format!("wirebond is not running here, and installing it needs {how} once"))
        };
    }
    let installed = match h.install(s) {
        Ok(changed) => changed,
        Err(e) => return Reply::fail("failed", format!("installing wirebond failed: {e}")),
    };
    let end = Instant::now() + h.start_wait();
    let c = loop {
        match h.dial(s, host::CTL_SOCK) {
            Ok(c) => break c,
            Err(_) if Instant::now() < end => thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Reply::fail("no_daemon", format!("wirebond is installed but its daemon does not answer: {e}"));
            }
        }
    };
    let mut r = exchange(c, req);
    if installed {
        // What only the CLI knows: that it installed, and where wirebond is now.
        r.changed |= r.ok;
        let path =
            if h.on_path(s) { String::new() } else { format!("; it is not on PATH: run it as {}", invoke(h, &exe)) };
        let line = format!("wirebond is installed as a system service at {}{path}", exe.display());
        r.message = if r.message.is_empty() { line } else { format!("{}\n{line}", r.message) };
    }
    r
}

/// One request line out, one reply line back (§20.1), within the verb's wait.
fn exchange(mut c: Box<dyn LocalStream>, req: &Request) -> Reply {
    let wait = match req {
        Request::Pick { .. } => Duration::from_secs(900), // ten candidates of a long --seconds
        Request::Update => Duration::from_secs(240),      // a check, a download and a smoke test
        _ => Duration::from_secs(30),
    };
    let late = |e: io::Error| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => format!("the daemon did not answer within {wait:?}"),
        _ => format!("the daemon's connection failed: {e}"),
    };
    let mut line = serde_json::to_vec(req).unwrap_or_default();
    line.push(b'\n');
    let mut raw = String::new();
    let sent = c.set_timeout(Some(wait)).and_then(|()| c.write_all(&line));
    if let Err(e) = sent.and_then(|()| BufReader::new(c.take(MAX_MSG as u64)).read_line(&mut raw).map(|_| ())) {
        return Reply::fail("failed", late(e));
    }
    match serde_json::from_str::<Reply>(&raw) {
        Ok(r) if !r.ok && r.error.is_none() => Reply::fail("failed", "the daemon failed without saying why"),
        Ok(r) => r,
        Err(e) => Reply::fail("failed", format!("the daemon's reply does not read: {e}")),
    }
}

/// `wirebond uninstall` (§22.2): needs administrator rights.
fn uninstall(args: &[String], h: &dyn Host) -> Reply {
    if !h.admin() {
        let msg = format!("run: {}", again(h, &h.current_exe(), args));
        return Reply { message: msg, ..Reply::fail("admin", "uninstalling wirebond needs administrator rights") };
    }
    match h.uninstall(h.scope()) {
        Ok((changed, step)) => {
            let what = if changed { "wirebond is uninstalled" } else { "wirebond was not installed" };
            let step = if step.is_empty() { String::new() } else { format!("\nstill to do: {step}") };
            Reply { changed, ..Reply::ok(format!("{what}{step}")) }
        }
        Err(e) => Reply::fail("failed", format!("uninstalling wirebond failed: {e}")),
    }
}

/// A changing verb's confirmation: what is true now, then the daemon's and the CLI's notes.
fn confirm(req: &Request, r: &Reply) -> String {
    let line = match req {
        Request::Client { anchor, mode } => {
            let mode = mode.map(|m| format!(" in {} mode", m.as_str())).unwrap_or_default();
            format!("this device is a client of {anchor}{mode}; wirebond status shows the bond")
        }
        Request::ClientOff => "the bond is off; traffic goes direct".into(),
        Request::Mode { mode } => format!("the bond's mode is {}", mode.as_str()),
        Request::PickAdd { device } => format!("{device} is a pick candidate"),
        Request::PickRemove { device } => format!("{device} is not a pick candidate"),
        Request::AnchorOn => {
            "this device accepts bonds from the devices it allows (wirebond anchor allow <device>)".into()
        }
        Request::AnchorOff => "this device accepts no bonds".into(),
        Request::AnchorAllow { devices } => format!("allowed: {}", devices.join(", ")),
        Request::AnchorDeny { devices } => format!("denied: {}", devices.join(", ")),
        Request::SupportOn { uplink, cable, for_client } => {
            let to = for_client.as_ref().map(|c| format!(" for {c}")).unwrap_or_default();
            format!("sharing {uplink} to {cable}{to}")
        }
        Request::SupportOff => "sharing is off".into(),
        Request::Update => "update checked".into(),
        Request::Pick { .. } => "pick finished".into(),
        Request::Status { .. } => String::new(),
    };
    let line = render::safe(&line);
    // update's message says what happened in place of the line.
    let line = if matches!(req, Request::Update) && !r.message.is_empty() { String::new() } else { line + "\n" };
    let unchanged = if r.changed || matches!(req, Request::Update) { "" } else { "(no change)\n" };
    line + unchanged + &note(&r.message)
}

/// The exact command that runs `args` again with the executable `exe`, as an administrator: with
/// sudo on macOS and Linux, after & in PowerShell, each word quoted where its shell needs it.
fn again(h: &dyn Host, exe: &Path, args: &[String]) -> String {
    let sudo = if h.windows() { "" } else { "sudo " };
    let plain = |a: &String| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c));
    let words = args.iter().map(|a| if plain(a) { a.clone() } else { quote(h, a) });
    words.fold(sudo.to_string() + &invoke(h, exe), |cmd, w| cmd + " " + &w)
}

/// How a person runs the executable `exe` in this OS's shell, its path quoted.
fn invoke(h: &dyn Host, exe: &Path) -> String {
    let amp = if h.windows() { "& " } else { "" };
    amp.to_string() + &quote(h, &exe.to_string_lossy())
}

/// `s` in single quotes, inside which sh, zsh and PowerShell take every character as it is but
/// the quote itself.
fn quote(h: &dyn Host, s: &str) -> String {
    let q = if h.windows() { "''" } else { r"'\''" };
    format!("'{}'", s.replace('\'', q))
}
