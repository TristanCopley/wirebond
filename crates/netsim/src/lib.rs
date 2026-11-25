//! wirebond's deterministic network simulator. It drives two real wirebond_core::Sessions through
//! simulated links in simulated time (SPEC §4.2). Tests only.
//!
//! The crate is one [`World`] that owns everything: two [`Device`]s (the node stand-in), every
//! [`Link`] and its two [`Dir`]ections, the event queue, the datagram registry, the checker, the
//! adversary and the traffic models. Everything else names a part of it by a small `Copy` id
//! ([`DevId`], [`LinkId`], [`DirId`], [`StreamId`], [`MoonId`], [`BulkId`]) and
//! reads it through `world[id]`. Scheduled events are closures that receive `&mut World`.

#![forbid(unsafe_code)]

pub mod apps;
pub mod check;
pub mod env;
pub mod fake;
pub mod link;
pub mod metrics;
pub mod nat;
pub mod node;
pub mod sim;
pub mod trace;

pub use apps::{
    AUDIO_SIZE, Bulk, BulkId, Echo, EchoId, Frame, Hdr, INPUT_SIZE, MoonId, Moonlight, PING_EVERY, PING_SIZE,
    PING_TIMEOUT, PORT_AUDIO, PORT_CONTROL, PORT_VIDEO, Sample, Stream, StreamId, VIDEO_SIZE,
};
pub use check::{ADV_KINDS, Adv, Adversary};
pub use env::{Check, Env, Eval, Opts};
pub use fake::{Evil, Fake, fake_maker};
pub use link::{Dir, DirId, Link, LinkId, LinkSpec, Profile, Side};
pub use metrics::{MESH_LANE, Recorder, WINDOW, late, pct, span};
pub use nat::{Nat, NatKind};
pub use node::{DevId, Device, MESH_SOCK, STUN_SERVER};
pub use sim::{
    Action, Actions, Dur, Endpoint, HOUR, Hook, HookArgs, HookKind, MIN, MS, Maker, NS, Rng, SEC, US, World, core_maker,
};
pub use trace::Trace;
