//! config.json v2 (SPEC 2.0 §22.4), the only persistent file: no keys. It lives in the state
//! directory, owned by root/SYSTEM, mode 0600. A field the file leaves out keeps its default, and
//! unknown fields are ignored. Owner: node daemon (notes/2.0-build.md); host only loads and saves.

use std::net::IpAddr;
use std::path::Path;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::api::{Mode, Role};
use crate::update;

/// Tailscale's address ranges: `mesh`'s default.
pub const DEFAULT_MESH: [&str; 2] = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"];
/// The default STUN servers (§9.2).
pub const DEFAULT_STUN: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// The bond subnets' defaults (§2): the anchor is .1 / ::1, clients from .2 / ::2.
pub const DEFAULT_BOND_NET4: &str = "10.77.0.0/24";
pub const DEFAULT_BOND_NET6: &str = "fd77:b0d::/64";

/// config.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The format: 2.
    pub v: u32,
    pub role: Role,
    /// A client's anchor: a MagicDNS name or a mesh address.
    pub anchor: Option<String>,
    pub mode: Mode,
    /// pick's candidates (`wirebond pick --add`).
    pub anchors: Vec<String>,
    /// An anchor's allow-list (`wirebond anchor allow`): device names or mesh addresses.
    pub allow: Vec<String>,
    /// A support's shared link.
    pub support: Option<SupportConfig>,
    pub stun: Vec<String>,
    pub bond_net4: String,
    pub bond_net6: String,
    /// The mesh's ranges, for meshes other than Tailscale.
    pub mesh: Vec<String>,
    /// The update's state (§22.3): its source and its trial.
    pub update: update::Config,
    /// macOS 1.x user scope: wirebond created ~/.local/bin. Goes with the login agent (§25; open).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub made_bin: bool,
}

/// `wirebond support on <uplink> <cable> [--for <client>]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportConfig {
    pub uplink: String,
    pub cable: String,
    #[serde(rename = "for")]
    pub for_client: Option<String>,
}

impl Default for Config {
    /// The config.json of a new install.
    fn default() -> Config {
        Config {
            v: 2,
            role: Role::None,
            anchor: None,
            mode: Mode::Performance,
            anchors: vec![],
            allow: vec![],
            support: None,
            stun: DEFAULT_STUN.map(String::from).into(),
            bond_net4: DEFAULT_BOND_NET4.into(),
            bond_net6: DEFAULT_BOND_NET6.into(),
            mesh: DEFAULT_MESH.map(String::from).into(),
            update: update::Config { url: update::DEFAULT_URL.into(), ..Default::default() },
            made_bin: false,
        }
    }
}

impl Config {
    /// Reads config.json from the state directory `dir`; a missing file gives the defaults. An
    /// unparsable file is an error and stays as it is.
    pub fn load(dir: &Path) -> io::Result<Config> {
        match fs::read(dir.join(super::CONFIG_FILE)) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes config.json to the state directory `dir` atomically, mode 0600.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let mut b = serde_json::to_vec_pretty(self)?;
        b.push(b'\n');
        super::install::put(&dir.join(super::CONFIG_FILE), &b, 0o600)
    }

    /// The mesh ranges for [`super::links`] and [`super::in_ranges`]; entries that are not an
    /// address and a prefix length are skipped.
    pub fn mesh_ranges(&self) -> Vec<(IpAddr, u8)> {
        self.mesh.iter().filter_map(|m| net(m)).collect()
    }

    /// The bond subnets (`bond_net4`, `bond_net6`) as given, each of its own family; None for an
    /// entry that is not one.
    pub fn bond_nets(&self) -> (Option<IpNet>, Option<IpNet>) {
        (net(&self.bond_net4).filter(|(a, _)| a.is_ipv4()), net(&self.bond_net6).filter(|(a, _)| a.is_ipv6()))
    }
}

/// An address and a prefix length.
pub type IpNet = (IpAddr, u8);

/// "address/prefix" as an address and a prefix length that fits it.
pub fn net(s: &str) -> Option<(IpAddr, u8)> {
    let (a, bits) = s.split_once('/')?;
    let (a, bits) = (a.parse::<IpAddr>().ok()?, bits.parse::<u8>().ok()?);
    (bits <= if a.is_ipv4() { 32 } else { 128 }).then_some((a, bits))
}
