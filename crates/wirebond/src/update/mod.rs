//! Self-update: check, fetch, verify, smoke test, swap, rollback and bad versions (SPEC §15.5).
//! Rust's standard library has no HTTP or TLS, so update runs the system's curl (macOS and Windows
//! 10 1803+ ship it, and the install line uses it on macOS and Linux) with explicit arguments,
//! never through a shell. Calls that take `&mut Config` change it, and node saves config.json
//! after each; the others never touch config.json. `exe` is the installed executable's path
//! (§15.3). Every error maps to the CLI's update_failed, with its message. node owns the timers
//! and the idle rule:
//!
//! 1. At every daemon start, before anything else: `back = started(&mut cfg.update, exe,
//!    version)`; save config.json; if `back`, `rollback(exe)` and, if that succeeds, `restart(exe)`.
//! 2. After UPDATE_HEALTHY with the event loop running and both local sockets answering (the mesh
//!    does not count): `healthy(&mut cfg.update, exe)`; save config.json.
//! 3. UPDATE_FIRST after start, then every UPDATE_EVERY ± UPDATE_JITTER, and at once when a peer's
//!    hello carries a version `newer` than this one (at most once per NEWER_PEER_CHECK): `check`,
//!    then `stage` a tag that is not already staged. Both block on the network, at most 85 s
//!    together, so node runs them off its event loop and never alongside `apply`. Ok from `stage`
//!    means the tag is staged: status shows update_ready.
//! 4. After UPDATE_IDLE with no flow on any connection, or at once on `wirebond update`:
//!    `apply(exe)`; if that succeeds, reply to the CLI, then `restart(exe)`.
//!
//! `stage` runs `<exe>.new --version --json`. The CLI answers it without contacting the daemon or
//! installing anything, by printing exactly this line on stdout and exiting 0, where version is
//! the build's version (the tag):
//!
//! ```text
//! {"ok":true,"data":{"version":"v0.3.1","commit":"<commit>"}}
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use std::{fs, io};
use wirebond_core::{
    Mono, SMOKE_TIMEOUT, UPDATE_EVERY, UPDATE_FIRST, UPDATE_HEALTHY, UPDATE_IDLE, UPDATE_JITTER, UPDATE_MAX_STARTS,
};

use crate::api::Reply;

#[cfg(test)]
mod tests;

/// The release origin.
pub const DEFAULT_URL: &str = "https://github.com/TristanCopley/wirebond";

/// Seconds each curl call may take, and the asset download (a 3 MB asset needs 0.6 Mbps): check
/// and stage together take at most 15 + 15 + 45 s plus the smoke test's 10, within node's 90 s and
/// the CLI's 4 minutes for update.
const MAX_TIME: &str = "15";
const ASSET_TIME: &str = "45";
/// SMOKE_TIMEOUT, which core keeps in nanoseconds.
const SMOKE: Duration = Duration::from_nanos(SMOKE_TIMEOUT as u64);

/// config.json's "update" object (§15.4).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The release origin; tests point it at a local server. Empty means DEFAULT_URL.
    pub url: String,
    /// Starts of the version on trial that have not yet reached healthy.
    pub starts: u32,
    /// The version on trial, which `starts` counts, so that a version installed during another's
    /// trial gets starts of its own (R1-13); empty outside a trial.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub trial: String,
    /// Versions that failed to start; never fetched again.
    pub bad: Vec<String>,
}

/// This platform's release asset name (§15.1), e.g. "wirebond-Darwin-arm64".
pub fn asset() -> &'static str {
    name(std::env::consts::OS, std::env::consts::ARCH)
}

/// A platform's asset, named by what `uname -sm` or Windows' PROCESSOR_ARCHITECTURE prints.
/// 32-bit ARM fetches armv6l, the one build, which releases also publish as armv7l.
fn name(os: &str, arch: &str) -> &'static str {
    match (os, arch) {
        ("macos", "aarch64") => "wirebond-Darwin-arm64",
        ("macos", "x86_64") => "wirebond-Darwin-x86_64",
        ("linux", "x86_64") => "wirebond-Linux-x86_64",
        ("linux", "aarch64") => "wirebond-Linux-aarch64",
        ("linux", "arm") => "wirebond-Linux-armv6l",
        ("windows", "x86_64") => "wirebond-Windows-AMD64.exe",
        ("windows", "aarch64") => "wirebond-Windows-ARM64.exe",
        _ => "wirebond-unsupported",
    }
}

/// Whether version `a` is newer than `b` (vX.Y.Z, with or without v; "dev" is never newer, and
/// nothing is newer than it). A release is newer than its own pre-releases, so that a device
/// running one still updates (R1-34), but a pre-release is never newer.
pub fn newer(a: &str, b: &str) -> bool {
    let (b, pre) = b.split_once('-').map_or((b, false), |(b, _)| (b, true));
    matches!((version(a), version(b)), (Some(x), Some(y)) if x > y || (pre && x == y))
}

/// Parses X.Y.Z after an optional v; printing it back rejects signs, leading zeros and suffixes.
fn version(s: &str) -> Option<[u64; 3]> {
    let s = bare(s);
    let mut n = s.splitn(3, '.').map(|p| p.parse().ok());
    let v = [n.next()??, n.next()??, n.next()??];
    (format!("{}.{}.{}", v[0], v[1], v[2]) == s).then_some(v)
}

/// A version without its v.
fn bare(v: &str) -> &str {
    v.strip_prefix('v').unwrap_or(v)
}

/// One check of the latest release (§15.5 step 1): one HEAD of <url>/releases/latest, whose
/// redirect it reads instead of following; the tag ends Location's path. None means nothing newer
/// (or no release yet). A redirect to anything but a release tag is an error, not silence
/// (R1-35), and so is a newer tag in `bad`, so that `wirebond update` says why it stays (R1-36).
pub fn check(cfg: &Config, running: &str) -> io::Result<Option<String>> {
    let url = at(cfg, "latest");
    // After the headers, -w prints the status and where the redirect goes.
    let out = String::from_utf8_lossy(&curl(&url, &["-I", "-w", "\n%{http_code} %{redirect_url}"])?).into_owned();
    let (_, last) = out.rsplit_once('\n').unwrap_or_default();
    let (code, to) = last.split_once(' ').unwrap_or_default();
    let to = to.trim_end_matches('/');
    let tag = match (code.starts_with('3'), to.rsplit_once("/releases/tag/")) {
        (true, Some((_, tag))) => tag,
        (true, None) if to.ends_with("/releases") => return Ok(None), // no release yet
        _ => return Err(io::Error::other(format!("HEAD {url}: {code}, Location {to:?}: not a release tag"))),
    };
    if newer(tag, running) && cfg.bad.iter().any(|b| bare(b) == bare(tag)) {
        return Err(io::Error::other(format!("{tag} is the latest but failed to start here (update.bad)")));
    }
    Ok(newer(tag, running).then(|| tag.to_owned()))
}

/// Fetches, verifies and stages `tag` as `<exe>.new`, then smoke-tests it (§15.5 steps 2–3). The
/// asset and SHA256SUMS both come from /releases/download/<tag>/, so from one release, and nothing
/// changes unless the asset's sha256 is on its SHA256SUMS line. Then a fresh `<exe>.new` replaces
/// any stale one (a new file, private until written, then mode 0755, synced; the installed file is
/// never written) and must answer `--version --json` with the tag within SMOKE_TIMEOUT, or it is
/// removed. Ok means `tag` is staged for `apply`.
pub fn stage(cfg: &Config, exe: &Path, tag: &str) -> io::Result<()> {
    let dir = at(cfg, &format!("download/{tag}/"));
    let sums = curl(&format!("{dir}SHA256SUMS"), &["-L"])?;
    let bin = curl(&format!("{dir}{}", asset()), &["-L", "--max-time", ASSET_TIME])?; // the last --max-time counts
    let sum: String = Sha256::digest(&bin).iter().map(|b| format!("{b:02x}")).collect();
    let line = format!("{sum}  {}", asset());
    if !String::from_utf8_lossy(&sums).lines().any(|l| l == line) {
        return Err(io::Error::other(format!("{} of {tag} does not match its SHA256SUMS line", asset())));
    }
    let next = with(exe, ".new");
    let _ = fs::remove_file(&next); // a new inode makes macOS check the signature anew
    let staged = crate::host::create(&next, &bin, 0o755).and_then(|()| smoke(&next, tag));
    staged.inspect_err(|_| _ = fs::remove_file(&next))
}

/// The smoke test (§22.3): `next version --json` must exit 0 within SMOKE_TIMEOUT and print
/// `wirebond version --json`'s object (`api::Version`) with the tag's version and a commit, not
/// merely a text containing it (R1-33). This catches a wrong architecture, a truncated file,
/// antivirus quarantine and a code-signing kill.
fn smoke(next: &Path, tag: &str) -> io::Result<()> {
    let why = match run(next) {
        Err(e) => e.to_string(),
        Ok(o) if !o.status.success() => o.status.to_string(),
        Ok(mut o) => {
            let v = serde_json::from_slice::<crate::api::Version>(&o.stdout).ok();
            if v.is_some_and(|v| bare(&v.version) == bare(tag) && !v.commit.is_empty()) {
                return Ok(());
            }
            o.stdout.truncate(100);
            format!("printed {:?}", String::from_utf8_lossy(&o.stdout))
        }
    };
    let what = format!("{} version --json must print {tag}", next.display());
    Err(io::Error::other(format!("smoke test: {what} within {}s: {why}", SMOKE.as_secs())))
}

/// Runs `next version --json`, killed if still running after SMOKE_TIMEOUT.
fn run(next: &Path) -> io::Result<Output> {
    let mut child = Command::new(next).args(["version", "--json"]).stdout(Stdio::piped()).spawn()?;
    let start = Instant::now();
    while child.try_wait()?.is_none() && start.elapsed() < SMOKE {
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill(); // past SMOKE_TIMEOUT; nothing if it exited
    child.wait_with_output()
}

/// Runs the system's curl for `url` with `args`, and returns what it printed, at most 64 MiB.
/// --disable skips any .curlrc. Plain HTTP is only for a test server on 127.0.0.1, which nothing
/// on the way can change and no proxy can reach; anything else is HTTPS, redirects included.
fn curl(url: &str, args: &[&str]) -> io::Result<Vec<u8>> {
    let proto = if url.starts_with("http://127.0.0.1:") && !url.contains('@') { "=http" } else { "=https" };
    let out = Command::new("curl")
        .args(["--disable", "--fail", "--silent", "--show-error"])
        .args(["--globoff", "--max-time", MAX_TIME, "--max-filesize"])
        .args(["67108864", "--proto", proto, "--proto-redir", proto])
        .args(args)
        .args(["--noproxy", "127.0.0.1", "--url", url])
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("curl: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(io::Error::other(format!("{url}: {} ({})", why.trim(), out.status)));
    }
    Ok(out.stdout)
}

/// The URL of `path` under the origin's releases.
fn at(cfg: &Config, path: &str) -> String {
    let url = Some(cfg.url.trim_end_matches('/')).filter(|u| !u.is_empty());
    format!("{}/releases/{path}", url.unwrap_or(DEFAULT_URL))
}

/// Swaps the staged file in (§15.5 step 5): `<exe>.new` becomes `<exe>`, and `<exe>` is kept as
/// `<exe>.old`, unless a version on trial already left a proven `<exe>.old`, which stays (R1-13).
pub fn apply(exe: &Path) -> io::Result<()> {
    let old = with(exe, ".old");
    swap(exe, &with(exe, ".new"), (!old.exists()).then_some(old))
}

/// Puts `<exe>.old` back over `<exe>` (§15.5 step 6).
pub fn rollback(exe: &Path) -> io::Result<()> {
    swap(exe, &with(exe, ".old"), None)
}

/// Moves `from` over `exe`, keeping `exe` as `keep` if given. On macOS and Linux `exe` is never
/// missing, even if the process dies midway (R1-32): `keep` is a hard link, and one rename
/// replaces `exe`. Windows can rename a running executable but not replace it, so `exe` moves to
/// `keep`, else to `<exe>.del` (which the next start removes), and back if `from` cannot follow.
fn swap(exe: &Path, from: &Path, keep: Option<PathBuf>) -> io::Result<()> {
    let aside = keep.or(cfg!(windows).then(|| with(exe, ".del")));
    match &aside {
        Some(aside) if cfg!(windows) => fs::rename(exe, aside)?,
        Some(aside) => fs::hard_link(exe, aside)?,
        None => {}
    }
    let moved = fs::rename(from, exe);
    match (&moved, &aside) {
        (Err(_), Some(aside)) if cfg!(windows) => _ = fs::rename(aside, exe),
        (Err(_), Some(aside)) => _ = fs::remove_file(aside),
        _ => {}
    }
    #[cfg(unix)]
    let _ = fs::File::open(exe.parent().unwrap_or(exe)).and_then(|dir| dir.sync_all());
    moved
}

/// Restarts into the new executable and never returns (§15.5 step 5). On macOS and Linux it execs
/// `exe` with the same arguments and environment: the process id stays, so launchd and systemd
/// see nothing. On Windows, or if the exec fails, it exits with status 3 without reporting
/// SERVICE_STOPPED, and the service manager starts `exe` again. Either way every socket closes
/// but ctl.sock and status.sock stay on disk, so the next daemon removes them before it binds.
#[cfg_attr(windows, allow(unused_variables))]
pub fn restart(exe: &Path) -> ! {
    #[cfg(unix)]
    let _ = std::os::unix::process::CommandExt::exec(Command::new(exe).args(std::env::args_os().skip(1)));
    std::process::exit(3)
}

/// At every daemon start: the rollback bookkeeping (§15.5 step 6), which changes `cfg`. It removes
/// a stale `<exe>.new` and `<exe>.del`. While `<exe>.old` exists the running version is on trial:
/// `starts` counts its starts, from 1 for a version new to the trial (R1-13), and once
/// UPDATE_MAX_STARTS of them ended without `healthy`, the next start adds it to `bad` (as a tag),
/// ends the trial and returns true: node rolls back.
pub fn started(cfg: &mut Config, exe: &Path, running: &str) -> bool {
    let _ = fs::remove_file(with(exe, ".new"));
    let _ = fs::remove_file(with(exe, ".del"));
    if !with(exe, ".old").exists() {
        (cfg.starts, cfg.trial) = (0, String::new()); // not on trial
        return false;
    }
    cfg.starts = if cfg.trial == running { cfg.starts + 1 } else { 1 };
    cfg.trial = running.to_owned();
    let back = cfg.starts > UPDATE_MAX_STARTS;
    if back {
        cfg.bad.push(format!("v{}", bare(running)));
        (cfg.starts, cfg.trial) = (0, String::new());
    }
    back
}

/// Ends a trial after UPDATE_HEALTHY healthy (§15.5 step 6), which changes `cfg`: node calls it
/// once per start. It resets the start count and deletes `<exe>.old`.
pub fn healthy(cfg: &mut Config, exe: &Path) {
    (cfg.starts, cfg.trial) = (0, String::new());
    let _ = fs::remove_file(with(exe, ".old"));
}

/// `exe` with `ext` appended, such as `<exe>.new`.
fn with(exe: &Path, ext: &str) -> PathBuf {
    let mut path = exe.as_os_str().to_owned();
    path.push(ext);
    path.into()
}

/// What the schedule asks of the daemon now (§22.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Check for a newer release, and stage it, off the loop.
    Check,
    /// The running version has been up UPDATE_HEALTHY: end its trial.
    Healthy,
    /// A staged version waits and the node has been idle UPDATE_IDLE (or `wirebond update`
    /// asked): swap it in and restart.
    Apply,
}

/// A `wirebond update` caller waiting for the outcome.
type Waiting = Sender<Reply>;
/// A check's outcome: the tag it staged, or None when nothing is newer.
type Checked = io::Result<Option<String>>;

/// §22.3's schedule, driven by the daemon's loop: a check UPDATE_FIRST after start, then every
/// UPDATE_EVERY ± UPDATE_JITTER, at once on `wirebond update` and when a peer's hello carries a
/// newer version (once per version); the trial ends healthy after UPDATE_HEALTHY; a staged
/// version is swapped in after UPDATE_IDLE without traffic, or at once on `wirebond update`.
/// Checks run off the loop, never two at once and never alongside a swap.
pub struct Schedule {
    /// The installed executable; None (tests, no service) never touches files.
    pub exe: Option<PathBuf>,
    next_check: Mono,
    checking: bool,
    staged: Option<String>,
    healthy: bool,
    start: Mono,
    /// The traffic counter as last seen, and when it last moved.
    idle: (u64, Mono),
    /// Peer versions that already asked for a check.
    peers: Vec<String>,
    waiting: Vec<Waiting>,
    /// `wirebond update` asked: swap in at once once staged.
    urgent: bool,
    rand: u64,
    done_tx: Sender<Checked>,
    done_rx: Receiver<Checked>,
}

impl Schedule {
    /// The schedule of a daemon started at `now`; `seed` draws the jitter.
    pub fn new(now: Mono, seed: u64) -> Schedule {
        let (done_tx, done_rx) = mpsc::channel();
        Schedule {
            exe: None,
            next_check: now + UPDATE_FIRST,
            checking: false,
            staged: None,
            healthy: false,
            start: now,
            idle: (0, now),
            peers: Vec::new(),
            waiting: Vec::new(),
            urgent: false,
            rand: seed | 1,
            done_tx,
            done_rx,
        }
    }

    /// The next check after one at `now`: UPDATE_EVERY ± UPDATE_JITTER, uniformly.
    fn next_after(&mut self, now: Mono) -> Mono {
        // xorshift64: the jitter needs no more.
        self.rand ^= self.rand << 13;
        self.rand ^= self.rand >> 7;
        self.rand ^= self.rand << 17;
        let span = 2 * UPDATE_JITTER as u64 + 1;
        now + UPDATE_EVERY - UPDATE_JITTER + (self.rand % span) as Mono
    }

    /// The version staged for the swap, if any.
    pub fn staged(&self) -> Option<&str> {
        self.staged.as_deref()
    }

    /// `wirebond update`: check at once, and swap in at once what it stages; `reply` hears the
    /// outcome.
    pub fn request(&mut self, now: Mono, reply: Waiting) {
        self.waiting.push(reply);
        self.urgent = true;
        if !self.checking {
            self.next_check = now;
        }
    }

    /// A peer's hello carried `theirs` while this node runs `running`: a newer version is checked
    /// for at once, once per version.
    pub fn peer(&mut self, now: Mono, running: &str, theirs: &str) {
        if newer(theirs, running) && !self.peers.iter().any(|p| p == theirs) {
            self.peers.push(theirs.to_owned());
            self.next_check = self.next_check.min(now);
        }
    }

    /// What is due at `now`, given the node's traffic counter (bytes sent and received so far).
    pub fn due(&mut self, now: Mono, traffic: u64) -> Option<Due> {
        if traffic != self.idle.0 {
            self.idle = (traffic, now);
        }
        if !self.healthy && now - self.start >= UPDATE_HEALTHY {
            self.healthy = true;
            return Some(Due::Healthy);
        }
        if self.checking {
            return None;
        }
        if self.staged.is_some() && (self.urgent || now - self.idle.1 >= UPDATE_IDLE) {
            return Some(Due::Apply);
        }
        if now >= self.next_check {
            self.checking = true;
            self.next_check = self.next_after(now);
            return Some(Due::Check);
        }
        None
    }

    /// A check ended: `Ok(Some(tag))` staged `tag`, `Ok(None)` found nothing newer. Returns the
    /// line for the log; callers of `wirebond update` hear it unless a swap follows.
    pub fn checked(&mut self, r: Checked) -> String {
        self.checking = false;
        let (ok, line) = match r {
            Ok(Some(tag)) => {
                self.staged = Some(tag.clone());
                return format!("update: {tag} staged");
            }
            Ok(None) => (true, "update: up to date".to_owned()),
            Err(e) => (false, format!("update failed: {e}")),
        };
        self.urgent = false;
        for w in self.waiting.drain(..) {
            let _ = w.send(if ok {
                Reply { ok, message: line.clone(), ..Reply::default() }
            } else {
                Reply::fail("failed", line.clone())
            });
        }
        line
    }

    /// Runs what is due: a check off the loop (calling `wake` when it ends), the end of the
    /// trial, or the swap and the restart. `cfg` is the node's config, saved in `dir` (if any)
    /// after a change. Returns lines for the log.
    pub fn tick(
        &mut self,
        now: Mono,
        traffic: u64,
        running: &str,
        cfg: &mut crate::host::Config,
        dir: Option<&Path>,
        wake: impl Fn() + Send + 'static,
    ) -> Vec<String> {
        let mut log = Vec::new();
        while let Ok(r) = self.done_rx.try_recv() {
            log.push(self.checked(r));
        }
        let Some(due) = self.due(now, traffic) else { return log };
        match (due, self.exe.clone()) {
            (Due::Check, Some(exe)) => {
                let (cfg, running, tx) = (cfg.update.clone(), running.to_owned(), self.done_tx.clone());
                let staged = self.staged.clone();
                std::thread::spawn(move || {
                    let r = check(&cfg, &running).and_then(|tag| match tag {
                        Some(t) if staged.as_deref() != Some(t.as_str()) => stage(&cfg, &exe, &t).map(|()| Some(t)),
                        other => Ok(other),
                    });
                    let _ = tx.send(r);
                    wake();
                });
            }
            (Due::Check, None) => log.push(self.checked(Ok(None))),
            (Due::Healthy, Some(exe)) => {
                if !cfg.update.trial.is_empty() || cfg.update.starts > 0 || with(&exe, ".old").exists() {
                    healthy(&mut cfg.update, &exe);
                    if let Some(Err(e)) = dir.map(|d| cfg.save(d)) {
                        log.push(format!("config.json: {e}"));
                    }
                    log.push(format!("update: {running} is healthy"));
                }
            }
            (Due::Healthy, None) => {}
            (Due::Apply, Some(exe)) => {
                let tag = self.staged.take().unwrap_or_default();
                match apply(&exe) {
                    Ok(()) => {
                        for w in self.waiting.drain(..) {
                            let message = format!("updated to {tag}: restarting");
                            let _ = w.send(Reply { ok: true, changed: true, message, ..Reply::default() });
                        }
                        restart(&exe);
                    }
                    Err(e) => log.push(self.checked(Err(io::Error::other(format!("swap to {tag}: {e}"))))),
                }
            }
            (Due::Apply, None) => self.staged = None,
        }
        log
    }

    /// At every daemon start, before anything else (§22.3): the trial's bookkeeping, saved
    /// through `save`; after UPDATE_MAX_STARTS failed starts the old version comes back and runs.
    pub fn boot(&mut self, cfg: &mut crate::host::Config, dir: Option<&Path>, running: &str) {
        let Some(exe) = self.exe.clone() else { return };
        let back = started(&mut cfg.update, &exe, running);
        if let Some(d) = dir {
            let _ = cfg.save(d);
        }
        if back && rollback(&exe).is_ok() {
            restart(&exe);
        }
    }
}
