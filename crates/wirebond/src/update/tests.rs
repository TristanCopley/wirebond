//! update's tests: a fake release origin on 127.0.0.1 (std::net), stand-in executables (shell
//! scripts, so the tests that run one need Unix), and the files beside a temporary `<exe>`.

use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// Held by every test that starts processes. A fork in one test while another has just written
/// its stand-in executable would make that one's exec fail with ETXTBSY.
static SPAWN: Mutex<()> = Mutex::new(());

fn spawning() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An answer of the fake origin.
enum Answer {
    Redirect(u16, String),
    Body(Vec<u8>),
}

/// A fake release origin: it answers each path from `routes`, 404 for any other, and logs each
/// request as "METHOD /path".
struct Origin {
    url: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl Origin {
    fn serve(routes: HashMap<String, Answer>) -> Origin {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut lines = BufReader::new(&conn).lines().map_while(Result::ok);
                let first = lines.next().unwrap_or_default();
                lines.take_while(|header| !header.is_empty()).for_each(drop);
                let mut words = first.split(' ');
                let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                seen.lock().unwrap().push(format!("{method} {path}"));
                let (head, body) = match routes.get(path) {
                    Some(Answer::Redirect(code, to)) => (format!("{code} Moved\r\nLocation: {to}"), &[][..]),
                    Some(Answer::Body(body)) => ("200 OK".to_owned(), &body[..]),
                    None => ("404 Not Found".to_owned(), &[][..]),
                };
                let head = format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let body = if method == "HEAD" { &[][..] } else { body };
                let _ = (&conn).write_all(&[head.as_bytes(), body].concat());
            }
        });
        Origin { url, log }
    }

    /// A release origin whose latest release redirects to `location`, with `files`.
    #[cfg(unix)]
    fn latest(location: &str, mut files: HashMap<String, Answer>) -> Origin {
        let to = Answer::Redirect(302, location.to_owned());
        files.insert("/releases/latest".to_owned(), to);
        Origin::serve(files)
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn config(&self) -> Config {
        Config { url: self.url.clone(), ..Config::default() }
    }
}

#[cfg(unix)]
fn sha256(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|b| format!("{b:02x}")).collect()
}

/// The files of release `tag` whose asset for this platform is `bin`, with a SHA256SUMS that
/// lists another asset first.
#[cfg(unix)]
fn release(tag: &str, bin: &str) -> HashMap<String, Answer> {
    let dir = format!("/releases/download/{tag}/");
    let sums = format!("{}  wirebond-Plan9-mips\n{}  {}\n", sha256(b""), sha256(bin.as_bytes()), asset());
    HashMap::from([
        (format!("{dir}{}", asset()), Answer::Body(bin.into())),
        (format!("{dir}SHA256SUMS"), Answer::Body(sums.into())),
    ])
}

/// A stand-in wirebond whose `version --json` prints `out`; any other arguments fail.
#[cfg(unix)]
fn fake(out: &str) -> String {
    format!("#!/bin/sh\n[ \"$1 $2\" = 'version --json' ] || exit 2\necho '{out}'\n")
}

/// What the 2.0 CLI prints for `version --json` (§22.3, api::Version).
#[cfg(unix)]
fn answer(version: &str) -> String {
    format!(r#"{{"version":"{version}","commit":"5f3da9ce114682d861f7cb0ce7c3259db1b502e4"}}"#)
}

/// A directory of its own for one test, beside the test binary (a temporary directory may not
/// allow executables), removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.with_file_name(format!("update-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)] // whatever the umask
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        Dir(dir)
    }

    /// `<exe>`, installed with `body`.
    fn exe(&self, body: &str) -> PathBuf {
        let exe = self.0.join("wirebond");
        put(&exe, body);
        exe
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `path` with `body`, mode 0755.
fn put(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// `<exe>`, `<exe>.new` and `<exe>.old`: each one's contents, or - if it does not exist.
fn files(exe: &Path) -> String {
    let show = |ext| fs::read_to_string(with(exe, ext)).unwrap_or_else(|_| "-".into());
    [show(""), show(".new"), show(".old")].join(" | ")
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&fs::metadata(path).unwrap().permissions()) & 0o7777
}

#[test]
fn asset_names() {
    // §15.1's names. 32-bit ARM fetches armv6l, the same build as armv7l.
    for (os, arch, want) in [
        ("macos", "aarch64", "wirebond-Darwin-arm64"),
        ("macos", "x86_64", "wirebond-Darwin-x86_64"),
        ("linux", "x86_64", "wirebond-Linux-x86_64"),
        ("linux", "aarch64", "wirebond-Linux-aarch64"),
        ("linux", "arm", "wirebond-Linux-armv6l"),
        ("windows", "x86_64", "wirebond-Windows-AMD64.exe"),
        ("windows", "aarch64", "wirebond-Windows-ARM64.exe"),
        ("freebsd", "x86_64", "wirebond-unsupported"),
    ] {
        assert_eq!(name(os, arch), want, "{os}/{arch}");
    }
    assert_eq!(asset(), name(std::env::consts::OS, std::env::consts::ARCH));
}

#[test]
fn newer_compares_versions() {
    for (a, b, want) in [
        ("v0.3.1", "v0.3.0", true),
        ("v0.4.0", "v0.3.9", true),
        ("v0.10.0", "v0.9.0", true), // numbers, not strings
        ("v1.0.0", "v0.99.99", true),
        ("0.3.1", "v0.3.0", true), // the v is optional on either side
        ("v0.3.1", "0.3.0", true),
        ("v0.3.0", "v0.3.0", false),
        ("v0.3.0", "0.3.0", false),
        ("v0.2.9", "v0.3.0", false),
        ("v0.4.0", "dev", false), // a dev build never updates itself
        ("dev", "v0.3.0", false),
        ("v0.4.0-rc1", "v0.3.0", false), // a pre-release is never installed
        ("v0.4", "v0.3.0", false),
        ("v0.4.0.1", "v0.3.0", false),
        ("v0.04.0", "v0.3.0", false),
        ("v+1.0.0", "v0.3.0", false),
        ("vv1.0.0", "v0.3.0", false),
        ("v99999999999999999999.0.0", "v0.3.0", false),
        ("", "v0.3.0", false),
        ("releases", "v0.3.0", false),
        // R1-34: a device running a pre-release updates to that release or a later one.
        ("v0.4.0", "v0.4.0-rc1", true),
        ("v0.4.1", "v0.4.0-rc1", true),
        ("v0.3.9", "v0.4.0-rc1", false),
        ("v0.4.0-rc2", "v0.4.0-rc1", false),
    ] {
        assert_eq!(newer(a, b), want, "newer({a:?}, {b:?})");
    }
}

#[test]
fn default_url() {
    assert_eq!(at(&Config::default(), "latest"), format!("{DEFAULT_URL}/releases/latest"));
    let cfg = Config { url: "https://example.com/o/r/".into(), ..Config::default() };
    assert_eq!(at(&cfg, "download/v0.4.0/x"), "https://example.com/o/r/releases/download/v0.4.0/x");
}

#[test]
fn config_json() {
    // A trial's version is written only during one, and config.json from before it still loads.
    let json = |c: &Config| serde_json::to_string(c).unwrap();
    assert_eq!(json(&Config::default()), r#"{"url":"","starts":0,"bad":[]}"#);
    let trial = Config { starts: 2, trial: "v0.4.0".into(), ..Config::default() };
    assert_eq!(json(&trial), r#"{"url":"","starts":2,"trial":"v0.4.0","bad":[]}"#);
    let old: Config = serde_json::from_str(r#"{"url":"u","starts":1,"bad":["v0.3.5"]}"#).unwrap();
    assert_eq!((old.url.as_str(), old.starts, old.trial.as_str()), ("u", 1, ""));
    assert_eq!(serde_json::from_str::<Config>("{}").unwrap(), Config::default());
}

/// A case of check: the answer to HEAD /releases/latest (None: 404), the running version,
/// update.bad, and the tag (Ok) or a word of the error (Err).
type CheckCase = (Option<Answer>, &'static str, &'static [&'static str], Want);
type Want = Result<Option<&'static str>, &'static str>;

#[test]
fn check_reads_the_redirect() {
    let _spawn = spawning();
    let tag = "https://github.com/TristanCopley/wirebond/releases/tag/v0.4.0";
    let to = |to: &str| Some(Answer::Redirect(302, to.into()));
    let moved = Some(Answer::Redirect(301, "https://github.com/n/r/releases/latest".into()));
    let page = Some(Answer::Body(b"captive portal".into()));
    #[rustfmt::skip]
    let cases: [(&str, CheckCase); 19] = [
        ("absolute Location", (to(tag), "v0.3.0", &[], Ok(Some("v0.4.0")))),
        ("relative Location", (to("/o/r/releases/tag/v0.4.0"), "v0.3.0", &[], Ok(Some("v0.4.0")))),
        ("trailing slash", (to("/o/r/releases/tag/v0.4.0/"), "v0.3.0", &[], Ok(Some("v0.4.0")))),
        ("running without v", (to(tag), "0.3.0", &[], Ok(Some("v0.4.0")))),
        ("same version", (to(tag), "v0.4.0", &[], Ok(None))),
        ("older version", (to(tag), "v0.4.1", &[], Ok(None))),
        ("other bad version", (to(tag), "v0.3.0", &["v0.3.5"], Ok(Some("v0.4.0")))),
        ("bad, but not newer", (to(tag), "v0.4.1", &["v0.4.0"], Ok(None))),
        ("dev build", (to(tag), "dev", &[], Ok(None))),
        ("pre-release", (to("/o/r/releases/tag/v0.4.0-rc1"), "v0.3.0", &[], Ok(None))),
        ("running a pre-release", (to(tag), "v0.4.0-rc1", &[], Ok(Some("v0.4.0")))),
        ("no release yet", (to("/o/r/releases"), "v0.3.0", &[], Ok(None))),
        // R1-36: a release skipped as bad says so.
        ("bad version", (to(tag), "v0.3.0", &["v0.3.5", "v0.4.0"], Err("failed to start here"))),
        ("bad version without v", (to(tag), "v0.3.0", &["0.4.0"], Err("failed to start here"))),
        ("not found", (None, "v0.3.0", &[], Err("404"))),
        ("not a redirect", (page, "v0.3.0", &[], Err("200"))),
        // R1-35: a redirect anywhere but to a release tag is an error, not "nothing newer".
        ("renamed repository", (moved, "v0.3.0", &[], Err("301"))),
        ("other page", (to("/o/r/releases/tag"), "v0.3.0", &[], Err("not a release tag"))),
        ("no Location", (to(""), "v0.3.0", &[], Err("not a release tag"))),
    ];
    for (case, (latest, running, bad, want)) in cases {
        let mut routes = HashMap::new();
        if let Some(latest) = latest {
            routes.insert("/releases/latest".to_owned(), latest);
        }
        let origin = Origin::serve(routes);
        let mut cfg = origin.config();
        cfg.bad = bad.iter().map(|b| b.to_string()).collect();
        match (check(&cfg, running), want) {
            (Ok(got), Ok(want)) => assert_eq!(got.as_deref(), want, "{case}"),
            (Err(e), Err(word)) => {
                eprintln!("{case}: {e}"); // what update_failed says
                assert!(e.to_string().contains(word), "{case}: {e}");
            }
            (got, want) => panic!("{case}: check = {got:?}, want {want:?}"),
        }
        // One HEAD of latest: its redirect is read, not followed.
        assert_eq!(origin.requests(), ["HEAD /releases/latest"], "{case}");
    }
}

#[test]
fn check_needs_https() {
    // Plain HTTP only to a test server on 127.0.0.1: curl refuses anything else before it
    // connects, so this needs no network.
    let _spawn = spawning();
    for url in ["http://github.com/o/r", "http://127.0.0.1:1@example.com", "http://127.0.0.1.example.com:1"] {
        let cfg = Config { url: url.into(), ..Config::default() };
        let e = check(&cfg, "v0.3.0").unwrap_err().to_string();
        assert!(e.contains(r#"curl: (1) Protocol "http""#), "{url}: {e}");
    }
}

/// A whole update: check, stage, apply, the new version's first start, healthy.
#[cfg(unix)]
#[test]
fn update() {
    let _spawn = spawning();
    let bin = fake(&answer("v0.4.0"));
    // GitHub serves the asset from another host, so the download follows a redirect.
    let asset_path = format!("/releases/download/v0.4.0/{}", asset());
    let sums = format!("{}  {}\n", sha256(bin.as_bytes()), asset());
    let routes = HashMap::from([
        (asset_path.clone(), Answer::Redirect(302, "/release-assets/1".into())),
        ("/release-assets/1".into(), Answer::Body(bin.clone().into())),
        ("/releases/download/v0.4.0/SHA256SUMS".into(), Answer::Body(sums.into())),
    ]);
    let origin = Origin::latest("/TristanCopley/wirebond/releases/tag/v0.4.0", routes);
    let dir = Dir::new("update");
    let exe = dir.exe("v0.3.0");
    put(&with(&exe, ".new"), "stale");
    let mut cfg = origin.config();

    let tag = check(&cfg, "v0.3.0").unwrap().expect("a newer release");
    assert_eq!(tag, "v0.4.0");
    stage(&cfg, &exe, &tag).unwrap();
    assert_eq!(files(&exe), format!("v0.3.0 | {bin} | -"));
    assert_eq!(mode(&with(&exe, ".new")), 0o755);
    // Both downloads come from the tag's directory, never from latest.
    let sums = "GET /releases/download/v0.4.0/SHA256SUMS".to_owned();
    let want = ["HEAD /releases/latest".into(), sums, format!("GET {asset_path}"), "GET /release-assets/1".into()];
    assert_eq!(origin.requests(), want);

    apply(&exe).unwrap();
    assert_eq!(files(&exe), format!("{bin} | - | v0.3.0"));
    assert!(!started(&mut cfg, &exe, "v0.4.0"));
    assert_eq!((cfg.starts, cfg.trial.as_str()), (1, "v0.4.0"));
    healthy(&mut cfg, &exe);
    assert_eq!(files(&exe), format!("{bin} | - | -"));
    assert_eq!((cfg.starts, cfg.trial.as_str()), (0, ""));
}

/// Stages release v0.4.1 from `files` over a stale `<exe>.new`; returns the error and the files.
#[cfg(unix)]
fn stage_fails(case: &str, files: HashMap<String, Answer>) -> (String, String) {
    let origin = Origin::serve(files);
    let dir = Dir::new(case);
    let exe = dir.exe("v0.3.0");
    put(&with(&exe, ".new"), "stale");
    let e = stage(&origin.config(), &exe, "v0.4.1").expect_err(case).to_string();
    eprintln!("{case}: {e}"); // what update_failed says
    (e, self::files(&exe))
}

#[cfg(unix)]
#[test]
fn stage_fetch_failures_change_nothing() {
    let _spawn = spawning();
    let good = fake(&answer("v0.4.1"));
    let (asset, sums) = (format!("/releases/download/v0.4.1/{}", asset()), "/releases/download/v0.4.1/SHA256SUMS");
    let mut tampered = release("v0.4.1", &good);
    tampered.insert(asset.clone(), Answer::Body(format!("{good}#").into()));
    let mut unlisted = release("v0.4.1", &good);
    unlisted.insert(sums.into(), Answer::Body(format!("{}  wirebond-Plan9-mips\n", sha256(good.as_bytes())).into()));
    let (mut no_asset, mut no_sums) = (release("v0.4.1", &good), release("v0.4.1", &good));
    no_asset.remove(&asset);
    no_sums.remove(sums);
    for (case, files, want) in [
        ("hash-mismatch", tampered, "does not match its SHA256SUMS line"),
        ("asset-not-in-sums", unlisted, "does not match its SHA256SUMS line"),
        ("no-asset", no_asset, "404"),
        ("no-sums", no_sums, "404"),
    ] {
        let (e, files) = stage_fails(case, files);
        assert!(e.contains(want), "{case}: {e}");
        // Nothing changed: the stale <exe>.new is still there.
        assert_eq!(files, "v0.3.0 | stale | -", "{case}");
    }
}

#[cfg(unix)]
#[test]
fn stage_smoke_failures_remove_the_stage() {
    let _spawn = spawning();
    let json = |v: &str| fake(&answer(v));
    for (case, bin, want) in [
        ("wrong-version", json("v0.3.9"), r#"printed "{\"version\":\"v0.3.9\""#),
        // R1-33: the exact version, not a text that contains it.
        ("longer-version", json("v0.4.10"), "v0.4.10"),
        ("pre-release", json("v0.4.1-rc1"), "v0.4.1-rc1"),
        // 1.x's wrapper is not 2.0's answer; and a version without its commit is not a build's.
        ("1.x-wrapper", fake(r#"{"ok":true,"data":{"version":"v0.4.1"}}"#), "ok\\\":true"),
        ("no-commit", fake(r#"{"version":"v0.4.1"}"#), r#"printed "{\"version\":\"v0.4.1\"}\n""#),
        ("not-json", fake("wirebond v0.4.1"), r#"printed "wirebond v0.4.1\n""#),
        ("failing-exit", format!("{}exit 1\n", json("v0.4.1")), "exit status: 1"),
        // macOS's posix_spawn hands a file with no #! to /bin/sh, which exits 127; Linux refuses it.
        (
            "not-an-executable",
            "garbage".into(),
            if cfg!(target_os = "macos") { "exit status: 127" } else { "Exec format error" },
        ),
    ] {
        let (e, files) = stage_fails(case, release("v0.4.1", &bin));
        assert!(e.starts_with("smoke test: "), "{case}: {e}");
        assert!(e.contains("wirebond.new version --json must print v0.4.1 within 10s: "), "{case}: {e}");
        assert!(e.contains(want), "{case}: {e}");
        assert_eq!(files, "v0.3.0 | - | -", "{case}");
    }
}

#[cfg(unix)]
#[test]
fn stage_kills_a_smoke_test_that_hangs() {
    let _spawn = spawning();
    let start = Instant::now();
    let (e, files) = stage_fails("hangs", release("v0.4.1", "#!/bin/sh\nexec sleep 30\n"));
    let took = start.elapsed();
    assert!(e.contains("must print v0.4.1 within 10s: signal: 9 (SIGKILL)"), "{e}");
    assert!(took >= SMOKE && took < SMOKE + Duration::from_secs(5), "took {took:?}");
    assert_eq!(files, "v0.3.0 | - | -");
}

#[cfg(unix)]
#[test]
fn stage_accepts_the_version_without_v() {
    let _spawn = spawning();
    let origin = Origin::serve(release("v0.4.1", &fake(&answer("0.4.1"))));
    let dir = Dir::new("without-v");
    let exe = dir.exe("v0.3.0");
    stage(&origin.config(), &exe, "v0.4.1").unwrap();
}

/// Stages under the umask the daemon binds its sockets with (§15.3), which would leave a new file
/// without any execute bit. The umask belongs to the process, so this runs in a child test process.
#[cfg(unix)]
#[test]
fn stage_ignores_the_umask() {
    let _spawn = spawning();
    if std::env::var_os("WIREBOND_UPDATE_UMASK").is_none() {
        let test = std::env::current_exe().unwrap();
        let child = Command::new("/bin/sh")
            .args(["-c", r#"umask 177 && exec "$0" "$@""#])
            .arg(test)
            .args(["update::tests::stage_ignores_the_umask", "--exact", "--nocapture"])
            .env("WIREBOND_UPDATE_UMASK", "1")
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        return;
    }
    let origin = Origin::serve(release("v0.4.1", &fake(&answer("v0.4.1"))));
    let dir = Dir::new("umask");
    let exe = dir.exe("v0.3.0");
    stage(&origin.config(), &exe, "v0.4.1").unwrap();
    assert_eq!(mode(&with(&exe, ".new")), 0o755);
}

/// The rollback state machine across starts of a new version (§15.5 step 6): a version gets
/// UPDATE_MAX_STARTS starts to reach healthy, and the next start rolls it back.
#[test]
fn trial() {
    for (case, fails, running) in [
        ("healthy-at-once", 0, "v0.4.0"),
        ("two-failures-then-healthy", 2, "v0.4.0"),
        ("three-failures-then-rollback", 3, "v0.4.0"),
        ("three-failures-without-v", 3, "0.4.0"),
    ] {
        let dir = Dir::new(case);
        let exe = dir.exe("v0.3.0");
        put(&with(&exe, ".new"), "v0.4.0");
        apply(&exe).unwrap();
        let mut cfg = Config { bad: vec!["v0.3.5".into()], ..Config::default() };
        for i in 1..=fails {
            assert!(!started(&mut cfg, &exe, running), "{case}: start {i}");
            assert_eq!(cfg.starts, i, "{case}");
        }
        if fails < UPDATE_MAX_STARTS {
            assert!(!started(&mut cfg, &exe, running), "{case}: last start");
            healthy(&mut cfg, &exe);
            assert_eq!(files(&exe), "v0.4.0 | - | -", "{case}");
            assert!(!started(&mut cfg, &exe, running), "{case}: next start");
            assert_eq!((cfg.starts, cfg.bad.len()), (0, 1), "{case}");
            continue;
        }
        assert!(started(&mut cfg, &exe, running), "{case}: start {} did not roll back", fails + 1);
        assert_eq!((cfg.starts, cfg.trial.as_str()), (0, ""), "{case}");
        assert_eq!(cfg.bad, ["v0.3.5", "v0.4.0"], "{case}");
        rollback(&exe).unwrap();
        assert_eq!(files(&exe), "v0.3.0 | - | -", "{case}");
        // The old version starts: no trial.
        assert!(!started(&mut cfg, &exe, "v0.3.0"), "{case}");
        assert_eq!((cfg.starts, files(&exe).as_str()), (0, "v0.3.0 | - | -"), "{case}");
    }
}

/// R1-13: an update during a trial keeps the proven `<exe>.old`, and the new version gets starts
/// of its own, so the fix is not condemned by the broken version's count.
#[test]
fn update_during_a_trial() {
    let dir = Dir::new("during-trial");
    let exe = dir.exe("v0.3.0");
    put(&with(&exe, ".new"), "v0.4.0");
    apply(&exe).unwrap();
    let mut cfg = Config::default();
    for _ in 0..3 {
        assert!(!started(&mut cfg, &exe, "v0.4.0")); // it crashes after each
    }
    put(&with(&exe, ".new"), "v0.4.1");
    apply(&exe).unwrap();
    assert_eq!(files(&exe), "v0.4.1 | - | v0.3.0");
    assert!(!started(&mut cfg, &exe, "v0.4.1"));
    assert_eq!((cfg.starts, cfg.trial.as_str()), (1, "v0.4.1"));
    // If the fix fails too, the device rolls back to the proven version, not the broken one.
    for _ in 0..2 {
        assert!(!started(&mut cfg, &exe, "v0.4.1"));
    }
    assert!(started(&mut cfg, &exe, "v0.4.1"));
    assert_eq!(cfg.bad, ["v0.4.1"]);
    rollback(&exe).unwrap();
    assert_eq!(files(&exe), "v0.3.0 | - | -");
}

/// R1-13: the install line puts a new version in place during a trial without touching
/// config.json; that version starts its own count.
#[test]
fn install_line_during_a_trial() {
    let dir = Dir::new("install-line");
    let exe = dir.exe("v0.4.0");
    put(&with(&exe, ".old"), "v0.3.0");
    let mut cfg = Config { starts: 3, trial: "v0.4.0".into(), ..Config::default() };
    put(&exe, "v0.4.1");
    assert!(!started(&mut cfg, &exe, "v0.4.1"));
    assert_eq!((cfg.starts, cfg.trial.as_str()), (1, "v0.4.1"));
    assert!(cfg.bad.is_empty());
}

#[test]
fn started_cleans_up() {
    let dir = Dir::new("cleans-up");
    let exe = dir.exe("v0.3.0");
    put(&with(&exe, ".new"), "stale");
    put(&with(&exe, ".del"), "a version a swap set aside");
    let mut cfg = Config { starts: 2, trial: "v0.4.0".into(), ..Config::default() };
    assert!(!started(&mut cfg, &exe, "v0.3.0")); // no <exe>.old: not on trial
    assert_eq!((cfg.starts, cfg.trial.as_str()), (0, ""));
    assert_eq!(files(&exe), "v0.3.0 | - | -");
    assert!(!with(&exe, ".del").exists());
}

#[test]
fn swap_fails_and_changes_nothing() {
    let dir = Dir::new("swap-fails");
    let exe = dir.exe("v0.3.0");
    assert!(apply(&exe).is_err(), "apply without <exe>.new");
    assert!(rollback(&exe).is_err(), "rollback without <exe>.old");
    assert_eq!(files(&exe), "v0.3.0 | - | -", "the installed file must stay");
}

/// R1-32: on macOS and Linux, `<exe>` exists at every moment of an apply and a rollback, so a kill
/// or a power cut in between never leaves the service without an executable.
#[cfg(unix)]
#[test]
fn exe_is_never_missing() {
    let dir = Dir::new("never-missing");
    let exe = dir.exe("v0.3.0");
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watch = {
        let (exe, done) = (exe.clone(), done.clone());
        thread::spawn(move || {
            let mut missing = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                missing += usize::from(!exe.exists());
            }
            missing
        })
    };
    for _ in 0..300 {
        put(&with(&exe, ".new"), "v0.4.0");
        apply(&exe).unwrap();
        rollback(&exe).unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(watch.join().unwrap(), 0);
    assert_eq!(files(&exe), "v0.3.0 | - | -");
}

/// restart execs `<exe>` with the process's own arguments, keeping its process id. The exec
/// replaces the process, so this runs in a child test process that restarts into a script.
#[cfg(unix)]
#[test]
fn restart_execs() {
    if let Some(next) = std::env::var_os("WIREBOND_UPDATE_RESTART") {
        restart(Path::new(&next));
    }
    let _spawn = spawning();
    let dir = Dir::new("restart");
    let next = dir.exe("#!/bin/sh\necho \"pid $$: $*\"\n");
    let args = ["update::tests::restart_execs", "--exact", "--nocapture"];
    let child = Command::new(std::env::current_exe().unwrap())
        .args(args)
        .env("WIREBOND_UPDATE_RESTART", &next)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.ends_with(&format!("pid {pid}: {}\n", args.join(" "))), "{out}");
}

/// §22.3's schedule: the first check UPDATE_FIRST after start, then UPDATE_EVERY ± UPDATE_JITTER;
/// healthy once at UPDATE_HEALTHY; no second check while one runs.
#[test]
fn schedule_checks_first_then_daily_with_jitter() {
    let mut s = Schedule::new(0, 7);
    assert_eq!(s.due(UPDATE_HEALTHY - 1, 0), None);
    assert_eq!(s.due(UPDATE_HEALTHY, 0), Some(Due::Healthy));
    assert_eq!(s.due(UPDATE_FIRST - 1, 0), None, "healthy once");
    assert_eq!(s.due(UPDATE_FIRST, 0), Some(Due::Check));
    assert_eq!(s.due(UPDATE_FIRST + UPDATE_EVERY * 2, 0), None, "one check at a time");
    assert_eq!(s.checked(Ok(None)), "update: up to date");
    let next = s.next_check;
    assert!(
        (UPDATE_FIRST + UPDATE_EVERY - UPDATE_JITTER..=UPDATE_FIRST + UPDATE_EVERY + UPDATE_JITTER).contains(&next)
    );
    assert_eq!(s.due(next - 1, 0), None);
    assert_eq!(s.due(next, 0), Some(Due::Check));
    let (mut a, mut b) = (Schedule::new(0, 1), Schedule::new(0, 2));
    let (x, y) = (a.next_after(0), b.next_after(0));
    assert_ne!(x, y, "the jitter differs between nodes");
}

/// A staged version is swapped in after UPDATE_IDLE without traffic, not before.
#[test]
fn schedule_applies_after_update_idle_without_traffic() {
    let mut s = Schedule::new(0, 7);
    s.healthy = true;
    assert_eq!(s.due(UPDATE_FIRST, 10), Some(Due::Check));
    assert_eq!(s.checked(Ok(Some("v2.0.1".into()))), "update: v2.0.1 staged");
    assert_eq!(s.staged(), Some("v2.0.1"));
    let t = UPDATE_FIRST + UPDATE_IDLE - 1;
    assert_eq!(s.due(t, 10), None, "idle since UPDATE_FIRST only");
    assert_eq!(s.due(t + 1, 11), None, "traffic restarts the idle clock");
    assert_eq!(s.due(t + 1 + UPDATE_IDLE - 1, 11), None);
    assert_eq!(s.due(t + 1 + UPDATE_IDLE, 11), Some(Due::Apply));
}

/// `wirebond update` checks at once, swaps at once and hears the outcome; a newer peer asks for
/// one check per version.
#[test]
fn schedule_answers_update_now_and_newer_peers() {
    let mut s = Schedule::new(0, 7);
    s.healthy = true;
    let (tx, rx) = mpsc::channel();
    s.request(SEC_TEST, tx);
    assert_eq!(s.due(SEC_TEST, 0), Some(Due::Check));
    s.checked(Ok(Some("v2.1.0".into())));
    assert_eq!(s.due(SEC_TEST, 0), Some(Due::Apply), "urgent: no idle wait");
    let (tx, rx2) = mpsc::channel();
    s.request(SEC_TEST, tx);
    s.staged = None;
    s.checking = true;
    s.checked(Ok(None));
    assert!(rx.try_recv().unwrap().message.contains("up to date") || rx2.try_recv().is_ok());
    let mut s = Schedule::new(0, 7);
    s.healthy = true;
    s.peer(SEC_TEST, "2.0.0", "1.9.0");
    assert_eq!(s.due(SEC_TEST, 0), None, "an older peer asks for nothing");
    s.peer(SEC_TEST, "2.0.0", "2.1.0");
    assert_eq!(s.due(SEC_TEST, 0), Some(Due::Check));
    s.checked(Ok(None));
    s.peer(2 * SEC_TEST, "2.0.0", "2.1.0");
    assert_eq!(s.due(2 * SEC_TEST, 0), None, "once per version");
    let (tx, rx) = mpsc::channel();
    s.request(3 * SEC_TEST, tx);
    assert_eq!(s.due(3 * SEC_TEST, 0), Some(Due::Check));
    s.checked(Err(io::Error::other("offline")));
    let r = rx.try_recv().unwrap();
    assert!(!r.ok && r.error.unwrap().msg.contains("offline"));
}

const SEC_TEST: Mono = 1_000_000_000;
