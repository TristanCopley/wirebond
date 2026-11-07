//! Install and uninstall (§14.1, §15.2, §15.3, §15.6): the scope a verb needs, the executable
//! copied into place, config.json, and the service or agent.

use super::{CONFIG_FILE, Config, Scope, os};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

// The running program, the file install copies into place.
#[cfg(test)]
use super::tests::executable;
#[cfg(not(test))]
use std::env::current_exe as executable;

/// The scope a changing verb needs: the system service for every role on every OS, a LaunchDaemon
/// on macOS too (SPEC 2.0 §22.2, §25: the login agent is gone); `serve` is 1.x's. The daemon's own
/// is `need(false)`. A 1.x login agent on a Mac goes at the next system install (`install`).
pub fn need(_serve: bool) -> Scope {
    Scope::System
}

/// Whether `s`'s config.json exists, which install writes before registering the service and
/// uninstall removes after. Only macOS has a separate user scope.
pub(super) fn has(s: Scope) -> bool {
    s.paths().1.join(CONFIG_FILE).exists() && (s == Scope::System || cfg!(target_os = "macos"))
}

/// Whether `wirebond` on PATH is `s`'s executable: in its directory, or a link to it such as the
/// one macOS's install makes in /usr/local/bin. While it is not, the CLI names the executable's
/// full path (§15.2).
pub fn on_path(s: Scope) -> bool {
    let exe = s.paths().0;
    let (Some(name), Ok(real)) = (exe.file_name(), fs::canonicalize(&exe)) else { return false };
    let dirs = env::var_os("PATH").unwrap_or_default();
    env::split_paths(&dirs).any(|d| fs::canonicalize(d.join(name)).is_ok_and(|p| p == real))
}

/// `path`, a PATH value, with `dir` added at its end; None while it holds `dir` already. Entries
/// match as Windows matches them, in any case and with or without a trailing backslash, and every
/// other entry stays as it is.
#[cfg(any(windows, test))]
pub(super) fn path_with(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    let path = path.trim_end_matches(';');
    Some(if path.is_empty() { dir.to_owned() } else { format!("{path};{dir}") })
}

/// `path` without `dir`, as [`path_with`] matches it; None while it does not hold `dir`.
#[cfg(any(windows, test))]
pub(super) fn path_without(path: &str, dir: &str) -> Option<String> {
    if !path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    Some(path.split(';').filter(|e| !same_dir(e, dir)).collect::<Vec<_>>().join(";"))
}

#[cfg(any(windows, test))]
fn same_dir(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\').eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Puts wirebond in place for scope `s` and starts it (§15.2, §15.3); idempotent. Returns whether
/// anything changed.
///
/// It creates the state directory, copies the running executable to `s`'s path (see `place`),
/// creates config.json, records madeBin if it created the executable's directory, registers the
/// service or agent (on Windows also the firewall rule) and starts it, restarting it on a new
/// executable. Only then is the download removed, so a failed install keeps it to run again
/// (R2-21); a missing executable, as a crash in an update's swap leaves, is put back the same way.
/// A system install on a Mac replaces the user's login agent and keeps its connections. The CLI
/// calls it before a changing verb when it is not running from `s`'s executable or ctl.sock does
/// not answer. System needs admin.
pub fn install(s: Scope) -> io::Result<bool> {
    let (exe, dir) = s.paths();
    let made = fs::metadata(bin(&exe)).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
    os::mk_state(&dir)?; // first: on macOS it holds the system executable
    let (mut changed, download) = place(s, &exe)?;
    let fresh = !dir.join(CONFIG_FILE).exists();
    if fresh || made {
        // A new system install over a login agent takes the agent's config.json: serve keeps the
        // agent's connections.
        let from = if fresh && s == Scope::System && has(Scope::User) { Scope::User } else { s };
        Config { made_bin: made, ..Config::load(&from.paths().1)? }.save(&dir)?;
        changed = true;
    }
    if s == Scope::System && has(Scope::User) {
        uninstall(Scope::User)?; // stopped before the daemon takes its ports
        changed = true;
    }
    changed |= os::register(s, &exe, changed)?;
    if let Some(d) = download {
        os::drop(&d);
    }
    Ok(changed)
}

/// Copies the running executable to `exe` unless it is `exe` already, and returns whether `exe`
/// changed and the download to remove once installed. The copy is a new file beside `exe`,
/// renamed over it, so it belongs to this process and takes the directory's SELinux label and
/// inherited ACL; a renamed download would keep its owner's, which could replace a service that
/// runs as root or LocalSystem, or one that systemd may not execute. `exe` itself, which may be
/// running, is never written into. The same build changes nothing, so running the install line
/// twice is idempotent.
fn place(s: Scope, exe: &Path) -> io::Result<(bool, Option<PathBuf>)> {
    let src = executable()?;
    if fs::canonicalize(&src)? == fs::canonicalize(exe).unwrap_or_default() {
        return Ok((false, None));
    }
    let b = fs::read(&src)?;
    if fs::read(exe).is_ok_and(|old| old == b) {
        return Ok((false, Some(src)));
    }
    os::halt(s);
    fs::create_dir_all(bin(exe))?;
    put(exe, &b, 0o755)?;
    Ok((true, Some(src)))
}

/// Removes everything `install` created for scope `s` (§15.6): the service or agent, the firewall
/// rule, the state directory, the executable and its update leftovers, and the executable's
/// directory if wirebond created it and it is empty; idempotent. Returns whether anything changed
/// and a note for a person: on macOS, while the application firewall still lists the executable,
/// the command that removes it, which needs admin. On Windows a detached cmd deletes the
/// executable's directory about 2 s later, once the caller has exited.
pub fn uninstall(s: Scope) -> io::Result<(bool, String)> {
    let (exe, dir) = s.paths();
    let made = Config::load(&dir).is_ok_and(|c| c.made_bin);
    let (mut changed, note) = os::unregister(s, &exe)?;
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
        changed = true;
    }
    changed |= os::remove_exe(&exe)?;
    if made {
        let _ = fs::remove_dir(bin(&exe)); // only if empty
    }
    Ok((changed, note))
}

/// The directory of the executable `exe`.
fn bin(exe: &Path) -> &Path {
    exe.parent().unwrap_or(exe)
}

/// Replaces `path` with `b` atomically, with `mode` whatever the umask: a new file beside `path`
/// ([`create`]), renamed over it. Nothing else remains, even on failure.
pub(super) fn put(path: &Path, b: &[u8], mode: u32) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{:x}", RandomState::new().hash_one(path)));
    let r = create(Path::new(&tmp), b, mode).and_then(|()| fs::rename(&tmp, path));
    r.inspect_err(|_| _ = fs::remove_file(&tmp))
}

/// Creates the new file `path` holding `b`, private to this process's user until it does (as Go's
/// CreateTemp makes it), then `mode` whatever the umask, and syncs it: config.json and the unit
/// through [`put`], and update's `<exe>.new`. On Windows the directory's DACL decides.
#[cfg_attr(windows, allow(unused_variables))]
pub fn create(path: &Path, b: &[u8], mode: u32) -> io::Result<()> {
    let mut new = fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut new, 0o600);
    let mut f = new.write(true).create_new(true).open(path)?;
    f.write_all(b)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
    f.sync_all()
}
