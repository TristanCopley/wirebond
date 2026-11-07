//! What the CLI prints for people (SPEC 2.0 §19): status (this device, its links, the bond with
//! one row per lane, an anchor's bonds, a support's cable, contribution, the mesh, the problems
//! each with its next step, the Moonlight line and the events) and pick's table (§18). Every
//! string a daemon or a peer sent is shown with its control and invisible characters escaped
//! (`safe`), so a name cannot move the cursor, forge lines or reorder text. Owner: cli.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::api::{AnchorBond, Bond, Egress, Lane, LaneState, Mode, Pick, Problem, Role, Status, problem};

/// Status for a person. `remote` names the device whose read-only view this is (`wirebond status
/// <device>`): its next steps are for that device, and the Moonlight line is this device's only.
pub fn status_text(st: &Status, remote: Option<&str>) -> String {
    let st = &safe(st);
    let role = if st.role == Role::None { "no role" } else { st.role.as_str() };
    let mut s =
        format!("{} · {role} · wirebond {}{}\n", st.device, st.version, tag(remote.is_some(), " · remote view"));
    if st.role == Role::None && remote.is_none() {
        s += "  wirebond client <anchor>, anchor on, or support on <uplink> <cable> gives it one\n";
    }
    s += &links(st);
    if let Some(b) = &st.bond {
        s += &bond(b, &st.device);
        if remote.is_none()
            && let Some(addr) = b.anchor_addr4.as_ref().or(b.anchor_addr6.as_ref())
        {
            // §6.1: the anchor's own services are at its bond address.
            s += &format!("\nMoonlight: add the host {addr} to stream from {}\n", b.anchor);
        }
    }
    if let Some(a) = &st.anchor {
        let allow =
            if a.allow.is_empty() { "nobody yet (wirebond anchor allow <device>)".into() } else { a.allow.join(", ") };
        let uplink = a.uplink.as_deref().unwrap_or("none");
        s += &format!("\nanchor · uplink {uplink} · egress {} · allows {allow}\n", egress(a.egress));
        s += &a.bonds.iter().map(anchor_bond).collect::<String>();
        s += tag(a.bonds.is_empty(), "  no bonds\n");
    }
    if let Some(sp) = &st.support {
        let to = sp.client.as_ref().map(|c| format!(" for {c}")).unwrap_or_default();
        s += &format!("\nsupport · shares {} to {}{to} · carried {}\n", sp.uplink, sp.cable, bytes(sp.carried_bytes));
    }
    let c = &st.contribution;
    let since = if c.since.is_empty() { "start" } else { &c.since };
    s += &format!("\ncontribution · sent {} · received {} · since {since}\n", bytes(c.tx_bytes), bytes(c.rx_bytes));
    s += &section("mesh", &st.mesh, |p| {
        let bonds = format!("{} bond{}", p.bonds, tag(p.bonds != 1, "s"));
        format!("{}\t{}\t{bonds}\t{}", p.device, p.role.as_str(), if p.reachable { "reachable" } else { "unreachable" })
    });
    s += &problems(st, remote);
    s + &section("events", &st.events, Clone::clone)
}

/// The links: id, name, kind, group, up or down, and what else is true of each.
fn links(st: &Status) -> String {
    section("links", &st.links, |l| {
        let mut notes = vec![if l.up { "up" } else { "down" }.to_string()];
        notes.extend(l.metered.then(|| "metered".into()));
        notes.extend(l.captive.then(|| "captive portal: carries no lane".into()));
        notes.extend(l.support.as_ref().map(|d| format!("from {d}")));
        format!("{}\t{}\t{}\tgroup {}\t{}", l.id, l.name, l.kind.as_str(), l.group, notes.join(" · "))
    })
}

/// A client's bond: its line, the bond addresses of `device` and of the anchor, then one row per
/// lane.
fn bond(b: &Bond, device: &str) -> String {
    let (mode, state) = (b.mode.as_str(), b.state.as_str());
    let mut s = format!("\nbond to {} · {mode} · {state} · egress {}\n", b.anchor, egress(b.egress));
    let here = addrs(&b.addr4, &b.addr6);
    let there = addrs(&b.anchor_addr4, &b.anchor_addr6);
    if !here.is_empty() || !there.is_empty() {
        s += &format!("  addresses: {device} {} · {} {}\n", or(&here, "-"), b.anchor, or(&there, "-"));
    }
    s += &format!("  flows: {} live, {} small\n", b.flows, b.small_flows);
    s + &lanes(&b.lanes, b.mode, "  ")
}

/// One bond on an anchor: its client, mode, state, addresses and bytes, then its lanes.
fn anchor_bond(b: &AnchorBond) -> String {
    let (mode, state, at) = (b.mode.as_str(), b.state.as_str(), addrs(&b.addr4, &b.addr6));
    let (out, local) = (bytes(b.egressed_bytes), bytes(b.local_bytes));
    let head = format!("  bond from {} · {mode} · {state}\n", b.client);
    let more = format!("    addresses: {} · egressed {out} · delivered here {local}\n", or(&at, "-"));
    head + &more + &lanes(&b.lanes, b.mode, "    ")
}

/// One row per lane (§11.5, §20.4): delay (RTT), jitter (dev), loss, rate and share, then what
/// the mode measures: first arrivals and rescues in performance, reorder wait in bandwidth. The
/// one-way delays and tx/rx rates are in --json; a row stays within 100 columns.
fn lanes(ls: &[Lane], mode: Mode, indent: &str) -> String {
    if ls.is_empty() {
        return format!("{indent}no lanes yet\n");
    }
    let tail = if mode == Mode::Performance { "first\trescues" } else { "reorder p50/p99" };
    let mut rows = vec![format!("lane\tlinks\tgroup\tstate\trtt\tdev\tloss\trate\tshare\t{tail}")];
    for l in ls {
        let name = if l.id == 0 { "mesh".to_string() } else { l.links.join(" → ") };
        // A lane never measured shows no numbers rather than zeros.
        let fresh = matches!(l.state, LaneState::Punching | LaneState::Dormant) && l.rtt_ms == 0.0;
        let num = |s: String| if fresh { "-".to_string() } else { s };
        let pct = |x: f64| num(format!("{:.0}%", 100.0 * x));
        let end = match mode {
            Mode::Performance => format!("{}\t{}/min", pct(l.first_arrivals), l.rescues_per_min),
            Mode::Bandwidth => num(format!("{:.1}/{:.1}ms", l.reorder_p50_ms, l.reorder_p99_ms)),
        };
        let (rtt, dev) = (num(format!("{:.1}ms", l.rtt_ms)), num(format!("{:.1}ms", l.dev_ms)));
        let (loss, rate) = (num(format!("{:.1}%", 100.0 * l.loss)), num(format!("{:.0}Mbps", l.rate_mbps)));
        let (id, group, state, share) = (l.id, l.group, l.state.as_str(), pct(l.share));
        rows.push(format!("{id}\t{name}\t{group}\t{state}\t{rtt}\t{dev}\t{loss}\t{rate}\t{share}\t{end}"));
    }
    table(&rows, indent)
}

/// The problems, each with its next step: the daemon's, or else the one §17 gives its code.
fn problems(st: &Status, remote: Option<&str>) -> String {
    let anchor = st.bond.as_ref().map_or("<anchor>", |b| b.anchor.as_str());
    let on = remote.map(|d| format!(" (on {d})")).unwrap_or_default();
    section("problems", &st.problems, |p| {
        let (what, next) = explain(p, anchor);
        let link = p.link.as_ref().map(|l| format!(" on {l}")).unwrap_or_default();
        let what = if p.msg.is_empty() { what } else { p.msg.clone() };
        let what = if what.is_empty() { what } else { format!(": {what}") };
        let next = p.next.clone().unwrap_or(next);
        format!("{}{link}{what}\n  next{on}: {next}", p.code)
    })
}

/// What a problem means and what to do about it (§5, §7, §17, §22), for a problem whose `msg` or
/// `next` the daemon left empty.
fn explain(p: &Problem, anchor: &str) -> (String, String) {
    let link = p.link.as_deref().unwrap_or("the link");
    let (what, next): (&str, String) = match p.code.as_str() {
        problem::ANCHOR_UNREACHABLE => (
            "the anchor does not answer; traffic goes direct",
            format!("check {anchor} is on and on the mesh: wirebond status {anchor}"),
        ),
        problem::BOND_DOWN => (
            "every lane and the channel are down; traffic goes direct",
            "wait: capture returns when a lane validates".into(),
        ),
        problem::CAPTIVE_PORTAL => (
            "this link wants a login, which would go through the anchor",
            format!("wirebond client off, log in on {link}, then wirebond client {anchor}"),
        ),
        problem::MESH_ONLY => (
            "only the mesh lane carries the bond, which gains nothing over the mesh alone",
            "cable a phone or another network, or try another anchor: wirebond pick".into(),
        ),
        problem::LANES_BLOCKED => {
            ("no lane validated: UDP may be blocked", "allow UDP 7849 to and from both devices".into())
        }
        problem::NO_EGRESS => (
            "the anchor offers no internet: only its own services go through the bond",
            format!("check {anchor}'s uplink: wirebond status {anchor}"),
        ),
        problem::DNS_LOCAL => ("DNS stays with the local resolver", "install and enable systemd-resolved".into()),
        problem::MISSING_TOOL => (
            "a tool wirebond needs is missing",
            "install it (ip: iproute2, nft: nftables, nmcli: NetworkManager), then restart wirebond".into(),
        ),
        problem::MESH_CAPTURED => (
            "the mesh's own traffic entered the bond, so capture is off",
            "check the mesh binds its sockets to the physical interface".into(),
        ),
        problem::PEER_NEWER => ("a peer runs a newer major version", "wirebond update".into()),
        problem::CONFIG_INVALID => (
            "config.json does not read; the defaults are in use",
            "set the role again: wirebond client <anchor>, anchor on or support on".into(),
        ),
        _ => ("", "wirebond status --json shows the details".into()),
    };
    (what.into(), next)
}

/// pick's result (§18): one row per candidate (lanes, groups, score), then the winner.
pub fn pick_text(p: &Pick) -> String {
    let p = &safe(p);
    let (unit, best) = match p.mode {
        Mode::Performance => ("ms", "lowest score wins"),
        Mode::Bandwidth => ("Mbps", "highest score wins"),
    };
    let mut rows = vec!["device\tlanes\tgroups\tscore".to_string()];
    for r in &p.rows {
        let score = match r.score {
            Some(x) => format!("{x:.1} {unit}"),
            None => format!("last: {}", or(&r.reason, "no score")),
        };
        rows.push(format!("{}\t{}\t{}\t{score}", r.device, r.lanes, r.groups));
    }
    let mut s = format!("pick · {} · {best}\n", p.mode.as_str()) + &table(&rows, "  ");
    s += &match (&p.winner, p.applied) {
        (None, _) => "no winner: no candidate could bond\n".into(),
        (Some(w), true) => format!("winner: {w}, now the anchor; the bond reconnects, which breaks open connections\n"),
        (Some(w), false) => format!("winner: {w} (nothing changed; wirebond client {w} makes it the anchor)\n"),
    };
    s
}

fn egress(e: Egress) -> &'static str {
    match e {
        Egress::V4 => "IPv4",
        Egress::V6 => "IPv6",
        Egress::Both => "IPv4 and IPv6",
        Egress::None => "none",
    }
}

/// The addresses that are known, space separated.
fn addrs(a4: &Option<String>, a6: &Option<String>) -> String {
    [a4, a6].into_iter().flatten().cloned().collect::<Vec<_>>().join(" ")
}

/// A byte count for people: B, kB, MB, GB, TB (powers of 1000), about three digits.
pub fn bytes(n: u64) -> String {
    let units = ["kB", "MB", "GB", "TB"];
    let (mut x, mut unit) = (n as f64, "B");
    for u in units {
        if x < 1000.0 {
            break;
        }
        (x, unit) = (x / 1000.0, u);
    }
    match unit {
        "B" => format!("{n} B"),
        _ if x >= 99.95 => format!("{x:.0} {unit}"),
        _ => format!("{x:.1} {unit}"),
    }
}

/// A section: its name, then one indented line per item (tab-separated cells become columns), or
/// "none".
fn section<T>(name: &str, items: &[T], f: impl Fn(&T) -> String) -> String {
    if items.is_empty() {
        return format!("\n{name}: none\n");
    }
    let rows: Vec<String> = items.iter().map(f).collect();
    // A row may carry more lines after its first; they stay as they are, indented.
    let firsts: Vec<String> = rows.iter().map(|r| r.lines().next().unwrap_or("").to_string()).collect();
    let cols = table(&firsts, "  ");
    let mut s = format!("\n{name}\n");
    for (line, row) in cols.lines().zip(&rows) {
        s += &format!("{line}\n");
        for more in row.lines().skip(1) {
            s += &format!("  {more}\n");
        }
    }
    s
}

/// Tab-separated rows as columns two spaces apart, each line indented, the last cell as it is.
fn table(rows: &[String], indent: &str) -> String {
    let rows: Vec<Vec<&str>> = rows.iter().map(|r| r.split('\t').collect()).collect();
    let mut width = vec![0; rows.iter().map(Vec::len).max().unwrap_or(0)];
    for r in &rows {
        for (i, cell) in r.iter().enumerate().take(r.len().saturating_sub(1)) {
            width[i] = width[i].max(cell.chars().count() + 2);
        }
    }
    let row = |r: &Vec<&str>| {
        let (last, cells) = r.split_last().unwrap_or((&"", &[]));
        let cols: String = cells.iter().zip(&width).map(|(c, &n)| format!("{c:<n$}")).collect();
        format!("{indent}{cols}{last}").trim_end().to_string() + "\n"
    };
    rows.iter().map(row).collect()
}

/// `x` with each control or invisible character in its strings shown as an escape: what a daemon
/// or a peer sends cannot command the terminal.
pub fn safe<T: Serialize + DeserializeOwned + Default>(x: &T) -> T {
    fn clean(v: &mut Value) {
        match v {
            Value::String(s) if s.contains(hidden) => *s = s.chars().map(shown).collect(),
            Value::Array(a) => a.iter_mut().for_each(clean),
            Value::Object(m) => m.values_mut().for_each(clean),
            _ => {}
        }
    }
    let Ok(mut v) = serde_json::to_value(x) else { return T::default() };
    clean(&mut v);
    serde_json::from_value(v).unwrap_or_default()
}

/// One line of JSON with the characters `hidden` finds written as `\u` escapes, which JSON reads
/// back the same: what a peer sent cannot command a terminal through --json either. Every such
/// character sits inside a string, since compact JSON has no whitespace outside one.
pub fn json_line(x: &impl Serialize) -> String {
    let s = serde_json::to_string(x).unwrap_or_default();
    s.chars().map(|c| if hidden(c) { format!("\\u{:04x}", c as u32) } else { c.into() }).collect::<String>() + "\n"
}

/// A character a terminal acts on or draws invisibly: C0, DEL and C1 controls, bidirectional
/// controls, zero-width characters and line separators.
pub fn hidden(c: char) -> bool {
    let invisible = matches!(c, '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}');
    c.is_control() || invisible || c == '\u{61c}' || c == '\u{feff}'
}

/// `c` as a terminal should show it: itself, or its escape when it is hidden.
fn shown(c: char) -> String {
    if hidden(c) { c.escape_unicode().to_string() } else { c.into() }
}

/// `s` when `on`, else nothing.
fn tag(on: bool, s: &str) -> &str {
    if on { s } else { "" }
}

/// `s`, or `default` when `s` is empty.
fn or<'a>(s: &'a str, default: &'a str) -> &'a str {
    if s.is_empty() { default } else { s }
}
