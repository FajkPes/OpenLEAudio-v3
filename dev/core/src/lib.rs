//! OpenLEAudio - user-mode Bluetooth LE Audio host stack.
//!
//! The goal is full control over the LC3 stream: bitrate, sampling frequency,
//! frame duration, transport latency and CIS topology - all the parameters the
//! Microsoft LE Audio driver decides for you and never exposes.

#[cfg(windows)]
/// What this build is made of, in one line.
///
/// Worth stating rather than assuming. Version 2 replaced the pure-Rust codec
/// with Google's reference LC3 and took the stream configuration order from
/// Android, and none of that is visible from the outside: the program has the
/// same name, the same window and the same console as version 1. Somebody
/// looking at a log months from now needs to be able to tell which one produced
/// it, and so does anybody deciding whether a bug report belongs here or to 1.0.
pub const STACK_PROVENANCE: &str =
    "Google LC3 (liblc3) + Android BAP configuration order";

/// The name this build answers to. Version 1 is a separate program that must
/// keep working, so the two must not be confusable.
pub const BUILD_NAME: &str = "OpenLEAudio 2 - Google";

pub mod audio;
pub mod att;
pub mod bap;
pub mod bonding;
pub mod privacy;
pub mod controller;
pub mod environment;
pub mod google;
pub mod hci;
pub mod l2cap;
pub mod lc3;
pub mod link;
pub mod multipoint;
pub mod media;
pub mod negotiation;
pub mod iso_flow;
pub mod safety;
#[cfg(windows)]
pub mod session;
pub mod settings;
pub mod smp;
pub mod stream;
pub mod trace;
pub mod transport;
pub mod vcs;

#[cfg(windows)]
pub mod winusb;

pub use bap::{CodecCapabilities, CodecConfiguration, PacRecord, Preset, QosConfiguration};
pub use att::{AclReassembler, AttError, Characteristic, L2capFrame, ServiceRange, Uuid};
pub use controller::{Controller, ControllerError, DiscoveredDevice};
pub use safety::{OutputLimiter, SafetyViolation, WritePolicy};
pub use stream::{AudioEncoder, StreamPlan, Topology};
pub use link::{AudioCapabilities, HciPump, Link, LinkError};
pub use hci::{BdAddr, Event, LocalVersion};
pub use transport::{ControllerInfo, TransportError, UsbTransport};

pub mod preflight;

pub mod radio;
