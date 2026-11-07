//! The local API (SPEC 2.0 §20.1) and the status JSON (§20.2): the contract between the daemon
//! (node), the CLI and the viewer (`crates/view`, which reads [`Status`]). One JSON [`Request`] and
//! one JSON [`Reply`] per line, each at most MAX_MSG. Once 2.0.0 ships, field names and enum
//! values never change; fields are only added. A reader takes a missing field as its default.
//!
//! This file depends only on serde and wirebond-core, so the viewer can share it as it is.
//! Owner: cli and api rendering (notes/2.0-build.md); a change here is a contract change.

use serde::{Deserialize, Serialize};

pub use wirebond_core::Mode;

/// A node's role (§3.1): exactly one at a time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    None,
    Client,
    Anchor,
    Support,
}

impl Role {
    /// The JSON name, which the CLI prints too.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::None => "none",
            Role::Client => "client",
            Role::Anchor => "anchor",
            Role::Support => "support",
        }
    }
}

/// One request, the CLI's verbs (§19) that the daemon answers: `{"verb": "client", "anchor":
/// "xanth", "mode": "performance"}`. `uninstall`, `version` and `run` never reach the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Request {
    /// This node, or with `device` another node's read-only view over the mesh (`view`, §16).
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
    },
    /// Make this node a client of `anchor` and start its bond; `mode` None keeps the configured one.
    Client {
        anchor: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
    },
    /// Stop the bond; traffic goes direct.
    ClientOff,
    /// Change the bond's mode.
    Mode {
        mode: Mode,
    },
    /// Measure and select (§18): `devices` empty means config.anchors.
    Pick {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        devices: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u32>,
        #[serde(default)]
        dry_run: bool,
    },
    PickAdd {
        device: String,
    },
    PickRemove {
        device: String,
    },
    AnchorOn,
    AnchorOff,
    AnchorAllow {
        devices: Vec<String>,
    },
    AnchorDeny {
        devices: Vec<String>,
    },
    /// Share `uplink` to `cable` (§8); with `for_client`, register with that client.
    SupportOn {
        uplink: String,
        cable: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        for_client: Option<String>,
    },
    SupportOff,
    /// Check for an update now and apply it (§22.3).
    Update,
}

impl Request {
    /// Whether the verb changes anything: the CLI installs the service on the first one (§19).
    pub fn changes(&self) -> bool {
        !matches!(self, Request::Status { .. })
    }
}

/// The one reply line. `ok` false carries `error`; a verb's own data is in its field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reply {
    pub ok: bool,
    /// Whether a changing verb changed anything (every verb is idempotent).
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Box<Status>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pick: Option<Pick>,
    /// For a person: what happened, or what to do next.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl Reply {
    pub fn ok(message: impl Into<String>) -> Reply {
        Reply { ok: true, message: message.into(), ..Reply::default() }
    }

    pub fn fail(code: &str, msg: impl Into<String>) -> Reply {
        Reply { error: Some(ApiError { code: code.into(), msg: msg.into() }), ..Reply::default() }
    }
}

/// A refused or failed verb. Codes: the control protocol's (`version`, `not_allowed`,
/// `too_many_bonds`, `busy`, `unknown`, `bad_request`, §16), plus `failed`, `admin` and
/// `no_daemon`, and the CLI's own `usage`. The CLI maps them to exit codes (§19): the three
/// refusals 5, `admin` 4, `no_daemon` 3, `usage` and `bad_request` 2, anything else 1.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiError {
    pub code: String,
    pub msg: String,
}

/// `wirebond version --json`, which the update's smoke test also runs (§22.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    pub version: String,
    pub commit: String,
}

/// pick's result (§18): one row per candidate, then the winner.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pick {
    pub mode: Mode,
    pub rows: Vec<PickRow>,
    pub winner: Option<String>,
    /// The winner became the anchor (not --dry-run).
    pub applied: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PickRow {
    pub device: String,
    pub lanes: u32,
    pub groups: u32,
    /// Performance: ms, lowest best; bandwidth: Mbit/s, highest best. None scores last.
    pub score: Option<f64>,
    /// Why it scores last, for a person; "" otherwise.
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// Status (§20.2)

/// The status JSON (§20.2): this node, or a remote view (which omits flow details and public
/// addresses). Units are in field suffixes: `_ms`, `_mbps`, `_bytes`; a field without one is a
/// ratio or a count.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub device: String,
    pub role: Role,
    pub version: String,
    pub links: Vec<Link>,
    /// A client's bond; null otherwise.
    pub bond: Option<Bond>,
    /// An anchor's bonds; null otherwise.
    pub anchor: Option<AnchorView>,
    /// A support's cable; null otherwise.
    pub support: Option<SupportView>,
    pub contribution: Contribution,
    pub mesh: Vec<MeshPeer>,
    pub problems: Vec<Problem>,
    /// The last STATUS_EVENTS events, oldest first, each "RFC 3339 time: message".
    pub events: Vec<String>,
}

/// What kind of link it is (§16 `links`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Wifi,
    Ethernet,
    Usb,
    Cellular,
    #[default]
    Other,
}

impl LinkKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            LinkKind::Wifi => "wifi",
            LinkKind::Ethernet => "ethernet",
            LinkKind::Usb => "usb",
            LinkKind::Cellular => "cellular",
            LinkKind::Other => "other",
        }
    }
}

/// One of this node's links.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Link {
    pub id: u8,
    pub name: String,
    pub kind: LinkKind,
    /// Its bottleneck group (§8).
    pub group: u8,
    pub metered: bool,
    pub up: bool,
    /// A captive portal answered (§17): the link carries no lane.
    pub captive: bool,
    /// The support device that gives it, when one registered (§8).
    pub support: Option<String>,
}

/// A bond's lifecycle (§3.2, §17).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondState {
    /// Dialing the anchor, or hello and bond open in flight.
    #[default]
    Connecting,
    /// The bond is open and no lane is validated yet: no capture.
    Punching,
    /// At least one lane validated; capture installed.
    Up,
    /// Every lane and the channel down for FAIL_OPEN_AFTER: capture removed (fail open).
    Down,
}

impl BondState {
    pub const fn as_str(self) -> &'static str {
        match self {
            BondState::Connecting => "connecting",
            BondState::Punching => "punching",
            BondState::Up => "up",
            BondState::Down => "down",
        }
    }
}

/// What the anchor offers for the internet (§3.2 `bond ok`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Egress {
    V4,
    V6,
    Both,
    #[default]
    None,
}

impl Egress {
    pub const fn as_str(self) -> &'static str {
        match self {
            Egress::V4 => "v4",
            Egress::V6 => "v6",
            Egress::Both => "both",
            Egress::None => "none",
        }
    }
}

/// A client's bond.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bond {
    pub anchor: String,
    pub mode: Mode,
    pub state: BondState,
    pub addr4: Option<String>,
    /// Beyond §20.2's example, so left out while unknown (a Windows anchor gives none, §6.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addr6: Option<String>,
    /// Where the anchor's own services are: what Moonlight adds as the host (§6.1).
    pub anchor_addr4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_addr6: Option<String>,
    pub egress: Egress,
    pub lanes: Vec<Lane>,
    /// Live flows, and how many of them are small (§13).
    pub flows: u32,
    pub small_flows: u32,
}

/// The state of a lane (§11.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaneState {
    #[default]
    Punching,
    Up,
    Suspect,
    Down,
    Dormant,
}

impl LaneState {
    pub const fn as_str(self) -> &'static str {
        match self {
            LaneState::Punching => "punching",
            LaneState::Up => "up",
            LaneState::Suspect => "suspect",
            LaneState::Down => "down",
            LaneState::Dormant => "dormant",
        }
    }
}

/// One lane of a bond (§11.5).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lane {
    /// client link << 8 | anchor link; 0 is the mesh lane.
    pub id: u16,
    /// The client's and the anchor's link names.
    pub links: Vec<String>,
    pub group: u8,
    pub state: LaneState,
    pub rtt_ms: f64,
    pub owd_ms: f64,
    pub dev_ms: f64,
    pub loss: f64,
    /// The controller's rate.
    pub rate_mbps: f64,
    pub tx_mbps: f64,
    pub rx_mbps: f64,
    /// This lane's fraction of the bond's bytes.
    pub share: f64,
    /// Performance: the fraction of packets whose first copy came on this lane.
    pub first_arrivals: f64,
    /// Performance: rescues over the last minute (§11.5).
    pub rescues_per_min: u64,
    /// Bandwidth: reorder wait of packets that came on this lane. Beyond §20.2's example, so left
    /// out while zero (always, in performance mode).
    #[serde(skip_serializing_if = "zero")]
    pub reorder_p50_ms: f64,
    #[serde(skip_serializing_if = "zero")]
    pub reorder_p99_ms: f64,
    pub bytes_today: u64,
}

/// Whether a measure is zero: a field §20.2's example does not show is then left out.
fn zero(x: &f64) -> bool {
    *x == 0.0
}

/// An anchor's view: its bonds and its uplink (§20.2).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnchorView {
    pub bonds: Vec<AnchorBond>,
    /// The interface of the default route; NAT follows it (§6.2).
    pub uplink: Option<String>,
    pub egress: Egress,
    pub allow: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnchorBond {
    pub client: String,
    pub mode: Mode,
    pub state: BondState,
    pub addr4: Option<String>,
    pub addr6: Option<String>,
    pub egressed_bytes: u64,
    pub local_bytes: u64,
    pub lanes: Vec<Lane>,
}

/// A support's view (§8, §20.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportView {
    pub uplink: String,
    pub cable: String,
    /// The client it registered with (`--for`).
    pub client: Option<String>,
    pub carried_bytes: u64,
}

/// What this node contributes (§20.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contribution {
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    /// "start" (the daemon's) or an RFC 3339 time.
    pub since: String,
    /// A client's bytes on each of its links, with each link's share of the bond (§20.3); left
    /// out when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkBytes>,
}

/// A client's bytes on one of its links (§20.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkBytes {
    /// The link's name; "mesh" for the mesh lane.
    pub link: String,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    /// This link's fraction of the bond's bytes.
    pub share: f64,
}

/// Another node on the mesh, as its `view` answered.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshPeer {
    pub device: String,
    pub role: Role,
    pub bonds: u32,
    pub reachable: bool,
}

/// A problem (§17) and the command that fixes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Problem {
    /// One of the `problem` constants.
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// The command to run next, if one helps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// For a person, with the numbers.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub msg: String,
}

/// Problem codes (§5, §6, §7, §17, §22): never renamed.
pub mod problem {
    pub const ANCHOR_UNREACHABLE: &str = "anchor_unreachable";
    pub const BOND_DOWN: &str = "bond_down";
    pub const CAPTIVE_PORTAL: &str = "captive_portal";
    pub const MESH_ONLY: &str = "mesh_only";
    pub const LANES_BLOCKED: &str = "lanes_blocked";
    pub const NO_EGRESS: &str = "no_egress";
    pub const DNS_LOCAL: &str = "dns_local";
    pub const MISSING_TOOL: &str = "missing_tool";
    pub const MESH_CAPTURED: &str = "mesh_captured";
    pub const PEER_NEWER: &str = "peer_newer";
    pub const CONFIG_INVALID: &str = "config_invalid";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_read_as_the_cli_writes_them() {
        let r: Request = serde_json::from_str(r#"{"verb":"client","anchor":"xanth","mode":"bandwidth"}"#).unwrap();
        assert_eq!(r, Request::Client { anchor: "xanth".into(), mode: Some(Mode::Bandwidth) });
        let r: Request = serde_json::from_str(r#"{"verb":"client_off"}"#).unwrap();
        assert_eq!(r, Request::ClientOff);
        assert!(serde_json::from_str::<Request>(r#"{"verb":"mode","mode":"stable"}"#).is_err(), "1.x names are gone");
        let s = serde_json::to_string(&Request::Status { device: None }).unwrap();
        assert_eq!(s, r#"{"verb":"status"}"#);
    }

    #[test]
    fn status_writes_the_spec_names() {
        let st = Status {
            device: "mac".into(),
            role: Role::Client,
            bond: Some(Bond { anchor: "xanth".into(), state: BondState::Up, egress: Egress::Both, ..Bond::default() }),
            ..Status::default()
        };
        let v: serde_json::Value = serde_json::to_value(&st).unwrap();
        assert_eq!(v["role"], "client");
        assert_eq!(v["bond"]["mode"], "performance");
        assert_eq!(v["bond"]["state"], "up");
        assert_eq!(v["bond"]["egress"], "both");
        assert_eq!(serde_json::from_value::<Status>(v).unwrap(), st);
    }

    /// `json` without the whitespace outside its strings.
    fn compact(json: &str) -> String {
        let (mut out, mut quoted, mut escaped) = (String::new(), false, false);
        for c in json.chars() {
            match c {
                _ if escaped => escaped = false,
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => continue,
                _ => {}
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn status_writes_section_20_2_exactly() {
        // The example as SPEC.md has it, so the two cannot drift apart.
        let spec = include_str!("../../../SPEC.md");
        let from = spec.find("### 20.2 Status JSON").expect("§20.2 in SPEC.md");
        let block = spec[from..].split("```").nth(1).expect("§20.2's example");
        let st: Status = serde_json::from_str(block).expect("§20.2's example reads as a Status");
        assert_eq!(serde_json::to_string(&st).unwrap(), compact(block), "the same fields, values and order");
        let bond = st.bond.as_ref().unwrap();
        assert_eq!(
            (bond.state, bond.lanes[0].first_arrivals, bond.lanes[0].rescues_per_min),
            (BondState::Up, 0.71, 12)
        );
        // The additions beyond the example are written once they hold something.
        let more = Bond { addr6: Some("fd77:b0d::2".into()), ..bond.clone() };
        assert!(serde_json::to_string(&more).unwrap().contains(r#""addr6":"fd77:b0d::2""#));
    }

    #[test]
    fn as_str_is_the_json_name() {
        let name = |v: serde_json::Value| v.as_str().unwrap().to_string();
        for r in [Role::None, Role::Client, Role::Anchor, Role::Support] {
            assert_eq!(r.as_str(), name(serde_json::to_value(r).unwrap()));
        }
        for k in [LinkKind::Wifi, LinkKind::Ethernet, LinkKind::Usb, LinkKind::Cellular, LinkKind::Other] {
            assert_eq!(k.as_str(), name(serde_json::to_value(k).unwrap()));
        }
        for s in [BondState::Connecting, BondState::Punching, BondState::Up, BondState::Down] {
            assert_eq!(s.as_str(), name(serde_json::to_value(s).unwrap()));
        }
        for e in [Egress::V4, Egress::V6, Egress::Both, Egress::None] {
            assert_eq!(e.as_str(), name(serde_json::to_value(e).unwrap()));
        }
        for s in [LaneState::Punching, LaneState::Up, LaneState::Suspect, LaneState::Down, LaneState::Dormant] {
            assert_eq!(s.as_str(), name(serde_json::to_value(s).unwrap()));
        }
    }
}
