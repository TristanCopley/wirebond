//! The control protocol v2 (SPEC 2.0 §16): one JSON object per line, UTF-8, at most MAX_MSG, over
//! TCP PORT on the mesh addresses. The message type is in `t`. Unknown fields are ignored; an
//! unknown `t` is answered with `error` `unknown` ([`decode`] tells it from a malformed message).
//!
//! The engine never sees JSON (§4.2): the daemon turns `key`, `links` and `mode` into the
//! engine's control input (`wirebond_core::Msg`) and the engine's key messages back into [`Key`].
//! Owner: node daemon (notes/2.0-build.md).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use wirebond_core::{MAX_MSG, Mode};

use crate::api::{self, Egress, LinkKind, Role};

/// One control message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// First, by both sides.
    Hello(Hello),
    /// On any refusal; the sender closes the channel after a hello's.
    Error(Error),
    /// The lifecycle (§3.2), by `op`.
    Bond(Bond),
    /// My send key for one epoch: at bond open and on each new epoch.
    Key(Key),
    /// My links and candidates: at bond open and on each change.
    Links(Links),
    /// The client's new mode.
    Mode { mode: Mode },
    /// support → client, after `wirebond support on … --for`.
    Support(Support),
    /// During `pick` (§18), by `op`.
    Measure(Measure),
    /// From any mesh peer: a request without `status`, answered with it.
    View(View),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// The protocol major: [`wirebond_core::PROTOCOL`] (2). A mismatch gets `error` `version`.
    pub v: u32,
    /// Random per process: a new one from a mesh address ends that address's older bond (§10.5
    /// rule 7).
    pub inst: u64,
    pub name: String,
    pub role: Role,
    /// Semver of the release.
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    #[serde(default)]
    pub msg: String,
}

/// The error codes of §16.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Version,
    NotAllowed,
    TooManyBonds,
    /// A `pick` is in progress.
    Busy,
    Unknown,
    BadRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Bond {
    /// client → anchor. `measure`: `pick`'s temporary bond (§18 step 1), which the anchor keeps
    /// beside the client's own bond without touching it: no bond addresses, no capture, no
    /// egress. Absent means false.
    Open {
        mode: Mode,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        measure: bool,
    },
    /// anchor → client: the client's bond addresses, the MTU, the DNS address and the egress.
    Ok { addr4: Option<Ipv4Addr>, addr6: Option<Ipv6Addr>, mtu: u16, dns: IpAddr, egress: Egress },
    /// Either side, at teardown (the client after removing capture).
    Close,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Key {
    /// The key id the receiver selects the key by.
    pub rid: u16,
    /// 32 bytes, base64.
    #[serde(with = "wirebond_core::b64")]
    pub key: Vec<u8>,
    pub epoch: u64,
    /// The sender's next counter: the receiver drops packets sealed below it under a key it took
    /// fresh (§10.4 step 1, "the key's floor"). §16 does not list it yet: an open item.
    #[serde(default)]
    pub floor: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Links {
    pub links: Vec<LinkOffer>,
}

/// One link as `links` offers it (§9.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkOffer {
    /// 1–MAX_LINKS.
    pub id: u8,
    pub kind: LinkKind,
    #[serde(default)]
    pub metered: bool,
    pub group: u8,
    /// The support device's name, or null.
    #[serde(default)]
    pub support: Option<String>,
    /// This link carries the mesh's underlay (§8): the mesh lane belongs to its group.
    #[serde(default)]
    pub mesh: bool,
    /// At most MAX_CANDS "ip:port".
    #[serde(default)]
    pub cands: Vec<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Support {
    /// The cable's subnet, "a.b.c.d/n": the client names the link in it after the support.
    pub cable_subnet: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Measure {
    /// client → candidate: measure for `seconds`, pushing at most `bulk_bytes` of filler.
    Start { seconds: u32, bulk_bytes: u64 },
    /// candidate → client: per lane, and its own egress (§18 step 4).
    Result { lanes: Vec<MeasureLane>, egress_ms: f64 },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MeasureLane {
    pub id: u16,
    pub rtt_p50: f64,
    pub owd_p99: f64,
    pub dev: f64,
    pub loss: f64,
    pub mbps: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// The answer: the status JSON of §20.2 without flow destinations; absent in the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Box<api::Status>>,
}

/// The types [`decode`] knows: anything else is `unknown`.
const TYPES: [&str; 9] = ["hello", "error", "bond", "key", "links", "mode", "support", "measure", "view"];

/// One message as its line: JSON and a newline, or None if it would exceed MAX_MSG.
pub fn encode(m: &Msg) -> Option<Vec<u8>> {
    let mut b = serde_json::to_vec(m).ok()?;
    b.push(b'\n');
    (b.len() <= MAX_MSG).then_some(b)
}

/// One received line (without its newline) as a message. `Unknown` for a well-formed object
/// whose `t` is not one of §16's; `BadRequest` for anything else.
pub fn decode(line: &[u8]) -> Result<Msg, ErrorCode> {
    if line.len() > MAX_MSG {
        return Err(ErrorCode::BadRequest);
    }
    let v: serde_json::Value = serde_json::from_slice(line).map_err(|_| ErrorCode::BadRequest)?;
    match v.get("t").and_then(|t| t.as_str()) {
        Some(t) if TYPES.contains(&t) => serde_json::from_value(v).map_err(|_| ErrorCode::BadRequest),
        Some(_) => Err(ErrorCode::Unknown),
        None => Err(ErrorCode::BadRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(m: Msg) -> String {
        let b = encode(&m).unwrap();
        assert_eq!(decode(&b[..b.len() - 1]), Ok(m));
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn every_message_round_trips_with_the_spec_names() {
        let hello = Hello { v: 2, inst: 7, name: "mac".into(), role: Role::Client, version: "2.0.0".into() };
        assert!(round(Msg::Hello(hello)).starts_with(r#"{"t":"hello","v":2"#));
        let open = round(Msg::Bond(Bond::Open { mode: Mode::Performance, measure: false }));
        assert_eq!(open, "{\"t\":\"bond\",\"op\":\"open\",\"mode\":\"performance\"}\n");
        let temp = round(Msg::Bond(Bond::Open { mode: Mode::Bandwidth, measure: true }));
        assert_eq!(temp, "{\"t\":\"bond\",\"op\":\"open\",\"mode\":\"bandwidth\",\"measure\":true}\n");
        let ok = Bond::Ok {
            addr4: Some(Ipv4Addr::new(10, 77, 0, 2)),
            addr6: None,
            mtu: 1400,
            dns: IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1)),
            egress: Egress::Both,
        };
        assert!(round(Msg::Bond(ok)).contains(r#""egress":"both""#));
        round(Msg::Bond(Bond::Close));
        let key = round(Msg::Key(Key { rid: 9, key: vec![1; 32], epoch: 0, floor: 5 }));
        assert!(key.contains(r#""key":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=""#), "{key}");
        let link = LinkOffer {
            id: 1,
            kind: LinkKind::Wifi,
            group: 1,
            mesh: true,
            cands: vec!["192.0.2.1:7849".parse().unwrap()],
            ..LinkOffer::default()
        };
        assert!(round(Msg::Links(Links { links: vec![link] })).contains(r#""mesh":true"#));
        let old = decode(br#"{"t":"links","links":[{"id":1,"kind":"wifi","group":1}]}"#).unwrap();
        assert!(matches!(old, Msg::Links(l) if !l.links[0].mesh), "absent is false");
        round(Msg::Mode { mode: Mode::Bandwidth });
        round(Msg::Support(Support { cable_subnet: "192.168.2.0/24".into(), name: "pc".into() }));
        round(Msg::Measure(Measure::Start { seconds: 10, bulk_bytes: 25_000_000 }));
        round(Msg::Error(Error { code: ErrorCode::TooManyBonds, msg: String::new() }));
        round(Msg::View(View::default()));
    }

    #[test]
    fn unknown_types_and_fields() {
        assert_eq!(decode(br#"{"t":"later","x":1}"#), Err(ErrorCode::Unknown));
        assert_eq!(decode(br#"{"t":"bond","op":"open"}"#), Err(ErrorCode::BadRequest), "open needs a mode");
        assert_eq!(decode(b"not json"), Err(ErrorCode::BadRequest));
        let m = decode(br#"{"t":"mode","mode":"bandwidth","extra":[1,2]}"#).unwrap();
        assert_eq!(m, Msg::Mode { mode: Mode::Bandwidth }, "unknown fields are ignored");
        assert_eq!(decode(br#"{"t":"mode","mode":"combine"}"#), Err(ErrorCode::BadRequest), "1.x names are gone");
    }
}
