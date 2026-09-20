//! End-to-end session: from an idle adapter to audio in the headphones.
//!
//! This is the layer that ties everything together and the only one that knows
//! the whole sequence:
//!
//! ```text
//!   open adapter -> initialise -> scan -> connect -> pair -> read PACS
//!        -> plan the stream -> configure ASCS -> create CIS -> stream LC3
//! ```
//!
//! Every step reports what it did, because when this fails against real hardware
//! the useful question is always "how far did it get".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::audio::{AudioCapture, AudioError, AudioRender};
use crate::bap::Preset;
use crate::controller::{Controller, ControllerError, DiscoveredDevice};
use crate::hci::{self, BdAddr};
use crate::link::{AudioCapabilities, Link, LinkError};
use crate::safety::{self, OutputLimiter, SafetyViolation, WritePolicy};
use crate::smp;
use crate::stream::{AudioDecoder, AudioEncoder, EncodeError, StreamPlan};
use crate::transport::CommandStyle;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Controller(#[from] ControllerError),

    #[error(transparent)]
    Link(#[from] LinkError),

    #[error(transparent)]
    Audio(#[from] AudioError),

    #[error(transparent)]
    Encode(#[from] EncodeError),

    #[error(transparent)]
    Safety(#[from] SafetyViolation),

    #[error("no LE Audio device found while scanning")]
    NoDeviceFound,

    #[error("could not plan a stream: {0}")]
    Planning(#[from] crate::stream::PlanError),

    #[error("stream parameters rejected: {0}")]
    UnsafeParameters(&'static str),

    #[error("connection attempt did not complete")]
    ConnectFailed,

    #[error("pairing succeeded but encryption did not start")]
    EncryptionFailed,

    #[error("battery ATT read timed out; the connection must be reopened")]
    BatteryReadTimeout,

    #[error(transparent)]
    Pairing(#[from] crate::smp::SmpError),

    #[error(transparent)]
    Transport(#[from] crate::transport::TransportError),

    #[error("CIS {handle:#06x} se nepodarilo ustavit: status {status:#04x} ({})", crate::controller::status_name(*status))]
    CisFailed { handle: u16, status: u8 },

    #[error("controller did not establish any CIS channel")]
    NoCisEstablished,

    #[error("isochronni cesta pro audio: {0}")]
    IsoPath(String),

    #[error("audio CIS {handle:#06x} disconnected: reason {reason:#04x}; ACL was not reported disconnected")]
    AudioDisconnected { handle: u16, reason: u8 },

    #[error("protejsek spojeni ukoncil: {} (kod {reason:#04x})", crate::hci::disconnect_reason(*reason))]
    Disconnected { reason: u8 },

    #[error("the adapter stopped answering: no events are arriving from it any more")]
    AdapterSilent,

    #[error(
        "the controller stopped confirming sent audio for {seconds} s, so nothing          is reaching the headphones any more"
    )]
    DeliveryStalled { seconds: u64 },
}

/// How long a CIS may take to come up.
///
/// The controller's own attempt times out at five seconds; waiting a little
/// longer means the report says which channel failed rather than that we gave
/// up first.
const CIS_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(8);

type Result<T> = std::result::Result<T, SessionError>;

/// Settings safe to change while ISO playback is already running. The pipe
/// reader updates this shared value immediately; the audio loop observes it on
/// its next frame without touching GATT, ASE, CIS or codec configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveAudioConfig {
    pub monitor_enabled: bool,
    pub monitor_source: String,
    pub monitor_replace: bool,
    pub monitor_gain: f32,
    pub output_gain: f32,
    pub microphone_gain: f32,

    /// Left/right balance, from -1.0 (left only) through 0.0 to +1.0 (right only).
    ///
    /// Applied to the captured frame before encoding, so it works the same
    /// whether the stream carries one channel or two, and takes effect on the
    /// next frame - a balance control that needs a reconnect cannot be set by
    /// ear, which is the only way anybody sets one.
    pub balance: f32,
    /// None preserves SessionConfig for CLI callers; Some(None) disables polling.
    pub battery_poll_override: Option<Option<Duration>>,
    pub metrics_override: Option<MetricsLevel>,
}

impl Default for LiveAudioConfig {
    fn default() -> Self {
        Self {
            monitor_enabled: false,
            monitor_source: "default".into(),
            monitor_replace: false,
            monitor_gain: 1.0,
            output_gain: 1.0,
            microphone_gain: 1.0,
            balance: 0.0,
            battery_poll_override: None,
            metrics_override: None,
        }
    }
}

/// What the caller wants from this session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub preset: Preset,
    /// USB control-transfer addressing used for HCI commands.
    pub command_style: CommandStyle,
    /// Carry stereo on one CIS when the device allows it.
    ///
    /// False by default. One stream for both ears is the tidier arrangement and
    /// the plan builder will still use it for a device that has only one Sink
    /// ASE, because there is nowhere else to put the other channel. As a
    /// preference, though, it selects a path no hardware here has ever run: the
    /// reference headset publishes one channel per stream, so "prefer single"
    /// would only ever take effect on a device nobody has tested.
    pub prefer_single_cis: bool,
    /// Capture device name to look for; None picks the virtual cable.
    pub audio_device: Option<String>,
    /// Render endpoint for decoded headset microphone audio. None means the
    /// microphone may be monitored locally but is not published to Windows.
    pub microphone_target: Option<String>,
    pub microphone_gain: f32,
    /// Optional Windows capture endpoint mixed into or substituted for music.
    /// `headset` uses the decoded Source ASE instead of opening a PC endpoint.
    pub monitor_source: Option<String>,
    pub monitor_replace: bool,
    pub monitor_gain: f32,
    pub live_audio: Arc<RwLock<LiveAudioConfig>>,
    /// Starts attenuated. Raise only once a stream is known to sound right.
    pub limiter: OutputLimiter,

    /// How long a stream may be silent before the endpoints are released.
    ///
    /// Headphones worn all day spend most of it with nothing playing, and a
    /// silent stream is not free: it keeps both radios busy, and - the part
    /// that matters more - it keeps the headphones ours. A headset whose sink
    /// endpoints are enabled cannot be taken by a phone, so holding a silent
    /// stream open is what stops multipoint working from the other side.
    ///
    /// This used to only stop transmitting, keeping the endpoints. That left
    /// the stack waiting for the headset to announce it was wanted elsewhere,
    /// which it cannot do while we are holding the endpoints it would need in
    /// order to be wanted. The wait was for an event our own behaviour
    /// prevented.
    ///
    /// Releasing on silence is what Android does, and the connection stays up
    /// either way: taking the endpoints back is the ordinary Enable already
    /// sent on every stream start. `None` never releases, which pins the
    /// headphones to this machine.
    pub idle_timeout: Option<Duration>,

    /// What to do when the headphones walk out of range.
    pub reconnect: ReconnectPolicy,

    /// How long silence lasts before the headphones are handed back.
    ///
    /// Holding a configured stream open through silence keeps the headphones
    /// ours, and a phone that starts playing cannot have them: from the
    /// headset's point of view this host is still using it. Releasing the
    /// endpoints while leaving the connection up is what multipoint is, done
    /// from the correct side - and taking them back is the ordinary Enable we
    /// already send, so nothing vendor-specific is involved either way.
    ///
    /// `None` never yields, which is right for someone who wants the headphones
    /// pinned to this machine.
    /// Whether to hand the endpoints back when the headset says it is wanted
    /// elsewhere. The connection is kept either way, so taking them back costs
    /// a stream rebuild and nothing more.
    pub yield_when_asked: bool,

    /// How much the stack interrogates the controller while audio plays.
    ///
    /// Every question is an HCI command over USB and a moment of the
    /// controller's attention, and the answers only ever go to a display.
    /// Someone running on battery who is not debugging anything should be able
    /// to say so and get the airtime back.
    pub metrics: MetricsLevel,

    /// Sends the left channel to the second stream and vice versa.
    ///
    /// For devices whose ASEs are wired to a fixed earpiece regardless of the
    /// channel allocation they are given. Nothing published by the device says
    /// which way round it is, so this is a listening test with a switch - and a
    /// listening test is worthless if it needs a reconnect between the two
    /// states. Shared rather than copied so it can be flipped while the music
    /// plays and judged by ear on the spot.
    pub swap_channels: Arc<AtomicBool>,
    pub scan_duration: Duration,

    /// How long the link may go unheard before the controller declares it lost.
    ///
    /// This is the number that decides what happens when someone walks out of
    /// range: shorter than the trip, and the connection is over before they are
    /// back; longer, and the audio stops and then resumes. It costs nothing
    /// while the headphones are in range.
    pub link_timeout: Duration,

    /// Raised from outside to ask the headphones for their battery level now.
    ///
    /// The audio loop owns the ACL for as long as it runs, so nothing else can
    /// put a question on the link while music is playing. This is how a click on
    /// the battery indicator reaches the radio instead of being ignored until
    /// the next disconnection.
    pub battery_refresh: Arc<AtomicBool>,

    /// How often to ask unprompted, or `None` to rely on notifications alone.
    ///
    /// Notifications are free and arrive when the level actually moves, so this
    /// is off by default. Some headsets subscribe successfully and then never
    /// notify, which looks like a battery stuck at whatever it was on connect,
    /// and for those a slow poll is the only way to see the number move.
    pub battery_poll: Option<Duration>,

    /// How many connection events the headphones may skip once audio is running.
    ///
    /// Zero leaves the link exactly as it was negotiated. Anything higher is
    /// pure saving on the headphones' side: the ACL carries no audio, so the
    /// events being skipped are ones where neither side had anything to say.
    /// The cost is that a volume press or a battery change is noticed up to that
    /// many intervals later, which for a handful of events is tens of
    /// milliseconds and inaudible.
    pub idle_link_latency: u16,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            preset: Preset::WindowsDefault,
            command_style: CommandStyle::ClassDevice,
            prefer_single_cis: false,
            audio_device: None,
            microphone_target: None,
            microphone_gain: 1.0,
            monitor_source: None,
            monitor_replace: false,
            monitor_gain: 1.0,
            live_audio: Arc::new(RwLock::new(LiveAudioConfig::default())),
            limiter: OutputLimiter::default(),
            scan_duration: Duration::from_secs(10),
            // Long enough that a gap between two tracks does not cost a
            // teardown, short enough that reaching for a phone works without
            // waiting. Five minutes - the old value - meant the endpoints were
            // held through every realistic pause, so in practice they were
            // never released at all.
            idle_timeout: Some(Duration::from_secs(10)),
            reconnect: ReconnectPolicy::default(),
            // None. See the setting for why: handing the headphones back costs a
            // whole stream rebuild, and there is no way to know from here
            // whether anything wants them.
            yield_when_asked: true,
            metrics: MetricsLevel::default(),
            swap_channels: Arc::new(AtomicBool::new(false)),
            link_timeout: Duration::from_secs(10),
            battery_refresh: Arc::new(AtomicBool::new(false)),
            battery_poll: None,
            idle_link_latency: 0,
        }
    }
}

/// Progress reported as the session advances, so a UI or log can follow along.
#[derive(Debug, Clone)]
pub enum Progress {
    LinkState { summary: String },
    RadioPower(crate::radio::Power),
    AdapterReady { version: String, address: String },
    DeviceFound { name: String, address: String, address_type: u8, rssi: i8, le_audio: bool },
    Connected { handle: u16 },
    CapabilitiesRead { summary: String },
    StreamPlanned { summary: String },
    Streaming {
        frames: u64,
        backlog: usize,
        /// Isochronous transfers submitted and how many came back failed. A
        /// growing failure count is the difference between "encoding fine but
        /// the audio never leaves the adapter" and a genuine radio problem.
        iso_sent: u64,
        iso_failed: u64,
        backpressure_frames: u64,
        /// Intervals where Windows had produced no audio in time and silence was
        /// sent to keep the stream in step. This is the PC failing to keep up,
        /// and it sounds identical to a radio fault - so it is counted
        /// separately rather than left to be blamed on the link.
        underruns: u64,
        /// Level of each captured channel, in dBFS. Two identical numbers over
        /// real music mean the source is mono and nothing downstream can undo it.
        left_db: f32,
        right_db: f32,
        /// Energy in the bass, middle and top of the captured audio, in dBFS.
        /// Measured before the encoder, so a missing band here is missing at
        /// the source and nothing downstream can be blamed for it.
        bass_db: f32,
        mid_db: f32,
        treble_db: f32,
        /// Current controller-reported received signal strength for the ACL.
        rssi: Option<i8>,
        /// Packets the controller confirms it has sent, per isochronous channel.
        delivered: Vec<u64>,
        /// What the radio itself says about each isochronous channel.
        ///
        /// Empty until the controller has answered once. A controller that does
        /// not implement LE Read ISO Link Quality leaves this empty forever,
        /// which is the honest answer - better than a zero that reads as
        /// "nothing was lost".
        quality: Vec<crate::hci::IsoLinkQuality>,
    },
    /// The audio source the stream is reading from, and its exact format.
    CaptureReady { device: String, format: String },
    /// How much charge each battery the device publishes has left.
    ///
    /// One entry per Battery Service instance, in the order the device lists
    /// them - which for earbuds is conventionally left, right, then case.
    Battery { levels: Vec<u8> },
    /// A battery level was asked for over the air, and why.
    ///
    /// Worth reporting because it is the one question this stack asks
    /// unprompted. Someone watching their headphones' runtime is entitled to see
    /// every packet spent on a display.
    BatteryAsked { reason: &'static str },
    /// Nothing has been playing, so the stack stopped transmitting. The
    /// Silence lasted long enough that the endpoints are being handed back.
    ///
    /// The connection stays up. The isochronous streams do not: they are what a
    /// phone needs in order to play to these headphones, and holding them
    /// through silence is what stops it.
    Idle { after: Duration },
    /// Sound came back and transmission resumed.
    Resumed,
    /// The headphones asked to be let go - they stopped offering to play media
    /// to us, which is what a headset does when a phone takes it. The endpoints
    /// are handed back; the connection stays up so they can be taken again.
    Yielded,
    /// A captured frame did not survive screening and was silenced.
    ///
    /// Worth saying, because otherwise a muted burst is a hole in the audio
    /// with nothing anywhere to explain it.
    FrameRefused { total: u64 },
    /// The ACL link ended at the controller, with the Bluetooth reason code.
    /// Kept separate from a generic stop so callers can safely decide whether
    /// this was a lost link worth reconnecting or a local/user-requested stop.
    Disconnected { reason: u8 },
    Stopped { reason: String },
}

/// How closely the stack watches its own radio while audio is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetricsLevel {
    /// Ask nothing. The stream still reports levels and frame counts, which
    /// cost nothing extra - they are measured from audio already in hand.
    Off,
    /// Signal strength only, once a second. One command, and it is the number
    /// that explains most dropouts.
    #[default]
    Signal,
    /// Signal strength and per-channel packet loss from the controller. The
    /// honest answer to "how much audio did the headphones miss", at the cost
    /// of one more command per channel per second.
    Full,
}

impl MetricsLevel {
    pub fn from_setting(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "signal" => Some(Self::Signal),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// What the stack does when a connection drops on its own.
///
/// Out of range is not an error to report and give up on - it is the normal
/// consequence of walking to the kitchen. The interval is deliberately not
/// aggressive: retrying every few hundred milliseconds drains the headphones'
/// battery scanning for a host that is not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub enabled: bool,
    pub interval: Duration,
    /// How long to keep trying before giving up and waiting to be asked.
    ///
    /// `None` never gives up. A bounded window is the better default: if the
    /// headphones have been out of range for two minutes they were taken off,
    /// not carried into the next room, and a host that keeps calling into an
    /// empty room is just spending battery on both ends.
    pub window: Option<Duration>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            // One second, not three. The radio does the waiting inside a single
            // attempt - it listens until the headphones advertise - so the gap
            // between attempts is pure delay on top of that, and three seconds
            // of it is what made reconnecting feel like the program had stopped
            // trying.
            interval: Duration::from_secs(1),
            window: Some(Duration::from_secs(900)),
        }
    }
}

impl ReconnectPolicy {
    pub fn disabled() -> Self {
        Self { enabled: false, ..Self::default() }
    }

    /// Keeps retrying for as long as the window allows.
    pub fn forever() -> Self {
        Self { window: None, ..Self::default() }
    }

    /// Whether another attempt should be made, `since_lost` after the drop.
    pub fn should_retry(&self, since_lost: Duration) -> bool {
        if !self.enabled {
            return false;
        }
        match self.window {
            Some(window) => since_lost < window,
            None => true,
        }
    }

    /// How many attempts the window allows, for showing to a person.
    pub fn attempts_in_window(&self) -> Option<u32> {
        let window = self.window?;
        let interval = self.interval.as_secs_f32().max(0.001);
        Some((window.as_secs_f32() / interval) as u32)
    }

    /// A reason code that means the link ended by itself rather than by us.
    ///
    /// Reconnecting after a local teardown would fight the user, who asked for
    /// the disconnection.
    pub fn worth_reconnecting(reason: u8) -> bool {
        const CONNECTION_TIMEOUT: u8 = 0x08;
        const REMOTE_TERMINATED: u8 = 0x13;
        const REMOTE_LOW_RESOURCES: u8 = 0x14;
        const REMOTE_POWER_OFF: u8 = 0x15;
        const LOCAL_HOST_TERMINATED: u8 = 0x16;

        match reason {
            LOCAL_HOST_TERMINATED | REMOTE_POWER_OFF => false,
            CONNECTION_TIMEOUT | REMOTE_TERMINATED | REMOTE_LOW_RESOURCES => true,
            _ => true,
        }
    }
}

/// True when a frame carries nothing a listener could hear.
///
/// Not a comparison against zero: a real silent stream from Windows carries
/// dither and the odd stray least significant bit, and treating that as audio
/// means never going idle at all.
/// Tilts an interleaved stereo frame towards one ear.
///
/// Only ever attenuates. Raising the near side instead would clip material that
/// is already near full scale, and a balance control that distorts at one end of
/// its travel is worse than no balance control.
pub fn apply_balance(samples: &mut [i16], balance: f32) {
    let balance = balance.clamp(-1.0, 1.0);
    if balance.abs() < 0.001 {
        return;
    }

    let left = if balance > 0.0 { 1.0 - balance } else { 1.0 };
    let right = if balance < 0.0 { 1.0 + balance } else { 1.0 };

    for pair in samples.chunks_exact_mut(2) {
        pair[0] = (pair[0] as f32 * left).round().clamp(-32768.0, 32767.0) as i16;
        pair[1] = (pair[1] as f32 * right).round().clamp(-32768.0, 32767.0) as i16;
    }
}

fn next_battery_index(batteries: &[(crate::link::BatteryHandles, Option<u8>)], start: usize, manual: bool) -> Option<usize> {
    (0..batteries.len()).map(|offset| (start + offset) % batteries.len())
        .find(|&index| manual || !batteries[index].0.notifies)
}

fn battery_read_error(payload: &[u8], handle: u16) -> bool {
    payload.len() == 5 && payload[0] == crate::att::att_op::ERROR_RESPONSE
        && payload[1] == crate::att::att_op::READ_REQUEST
        && u16::from_le_bytes([payload[2], payload[3]]) == handle
}

fn silence_release_timeout(timeout: Option<Duration>, microphone_active: bool) -> Option<Duration> {
    if microphone_active { None } else { timeout }
}

fn is_silent(samples: &[i16]) -> bool {
    const FLOOR: i16 = 16; // about -66 dBFS
    samples.iter().all(|s| s.saturating_abs() <= FLOOR)
}

fn resample_mono(source: &[i16], source_rate: u32, target_rate: u32, gain: f32) -> Vec<i16> {
    if source.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    let output_len = ((source.len() as u64 * target_rate as u64) / source_rate as u64)
        .max(1) as usize;
    (0..output_len)
        .map(|index| {
            let source_index = index * source.len() / output_len;
            (source[source_index] as f32 * gain.clamp(0.0, 2.0))
                .clamp(i16::MIN as f32, i16::MAX as f32) as i16
        })
        .collect()
}

fn same_virtual_cable(capture: &str, render: &str) -> bool {
    let family = |name: &str| {
        let name = name.to_ascii_lowercase();
        if name.contains("cable-a") {
            "a"
        } else if name.contains("cable-b") {
            "b"
        } else if name.contains("cable") || name.contains("vb-audio") {
            "plain"
        } else if name.contains("voicemeeter") {
            "voicemeeter"
        } else {
            "other"
        }
    };
    let capture_family = family(capture);
    capture_family != "other" && capture_family == family(render)
}

/// Pulls an ATT notification apart, if that is what this frame is.
fn notification(frame: &crate::att::L2capFrame) -> Option<(u16, &[u8])> {
    if frame.cid != crate::att::cid::ATT {
        return None;
    }

    let &[opcode, lo, hi, ref value @ ..] = frame.payload.as_slice() else {
        return None;
    };

    if opcode != crate::att::att_op::HANDLE_VALUE_NOTIFICATION {
        return None;
    }

    Some((u16::from_le_bytes([lo, hi]), value))
}

/// Keeps the volume on the headphones and the Windows slider showing one number.
///
/// In LE Audio the headphones own the volume, so a press on the earcup is the
/// authoritative event and Windows has to follow it. Without this the buttons
/// look broken: they do change the volume, but only inside the headphones,
/// while the Windows slider stays where it was.
pub struct VolumeBridge {
    handles: crate::link::VolumeControlHandles,
    state: crate::vcs::VolumeState,
    system: Option<crate::audio::SystemVolume>,
}

impl VolumeBridge {
    pub fn state(&self) -> crate::vcs::VolumeState {
        self.state
    }

    pub fn describe(&self) -> String {
        let muted = if self.state.muted { ", ztlumeno" } else { "" };
        let slider = match self.system {
            Some(_) => "the Windows volume slider will follow them",
            None => "shown here only; the Windows slider belongs to the cable and is left alone",
        };
        format!("hlasitost sluchatek {} %{muted}; {slider}", self.state.percent())
    }

    /// Applies a Volume State notification, reporting whether anything moved.
    fn absorb(&mut self, value: &[u8]) -> bool {
        let Some(new_state) = crate::vcs::parse_volume_state(value) else {
            return false;
        };
        if new_state == self.state {
            return false;
        }

        self.state = new_state;

        if let Some(system) = &self.system {
            // Mirror, do not translate: both sides are a fraction of full scale,
            // and inventing a curve between them is how the two stop agreeing.
            let _ = system.set_level(new_state.scalar());
            let _ = system.set_muted(new_state.muted);
        }

        true
    }
}

/// Which isochronous channels came up, and which did not.
///
/// A partial result is a real outcome, not a failure. One working ear beats
/// silence, and the whole reason this project exists is that Windows treats a
/// failed second CIS as a failed connection: it tears down the channel that did
/// work and retries the identical request, forever.
#[derive(Debug, Clone, Default)]
pub struct CisOutcome {
    pub established: Vec<u16>,
    /// Handle and status of each channel that refused to come up.
    pub failed: Vec<(u16, u8)>,
}

impl CisOutcome {
    /// True when every channel that was asked for came up.
    pub fn complete(&self) -> bool {
        !self.established.is_empty() && self.failed.is_empty()
    }

    /// A sentence for a person, naming what is missing.
    pub fn describe(&self) -> String {
        if self.complete() {
            return format!("{} channels established", self.established.len());
        }

        let failures: Vec<String> = self
            .failed
            .iter()
            .map(|(handle, status)| match status {
                0xFF => format!("{handle:#06x} did not respond"),
                _ => format!("{handle:#06x} status {status:#04x}"),
            })
            .collect();

        format!(
            "ustaveno {} z {}, nepovedlo se: {}",
            self.established.len(),
            self.established.len() + self.failed.len(),
            failures.join(", ")
        )
    }
}

/// A session in progress.
pub struct Session {
    pub peer_identity: Option<crate::privacy::PeerIdentity>,
    config: SessionConfig,
    controller: Option<Controller>,
    /// Isochronous channels the last group established.
    ///
    /// Kept because the tidy-up before building a new group has to disconnect
    /// them first, and by then the plan that created them is gone. Without this
    /// the pre-clean is a bare group removal, which the controller refuses
    /// precisely when it matters - when channels are still up - so the leftover
    /// survives and the next connection is turned away over it.
    last_cis_handles: Vec<u16>,
    write_policy: WritePolicy,
    volume: Option<VolumeBridge>,
    /// Each Battery Service the peer publishes, and the last level seen.
    batteries: Vec<(crate::link::BatteryHandles, Option<u8>)>,
    /// The Available Audio Contexts value handle, once discovery has found it.
    ///
    /// This is the headset's own announcement of what it can accept right now,
    /// and the only vendor-neutral one there is. Watching it is what turns
    /// handing the headphones back into an answer to a question the headset
    /// asked, rather than a guess made on a timer.
    contexts_handle: Option<u16>,
    /// Whether the headset is currently offering to play media to us.
    ///
    /// False means something else has it - in practice a phone. Starts true:
    /// a device that never publishes contexts must not be treated as busy.
    media_available: bool,
    active_context: u16,
    available_contexts: Option<u16>,
    /// Keep the wake-up frame separate from capture discontinuities during setup.
    resume_capture: Option<(String, u32, AudioCapture, Vec<i16>)>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            peer_identity: None,
            config,
            controller: None,
            last_cis_handles: Vec::new(),
            write_policy: WritePolicy::default(),
            volume: None,
            batteries: Vec::new(),
            contexts_handle: None,
            media_available: true,
            active_context: crate::bap::ascs::CONTEXT_MEDIA,
            available_contexts: None,
            resume_capture: None,
        }
    }

    /// Opens the adapter and brings the controller up.
    ///
    /// Fails clearly if the adapter is still owned by Windows, because that is
    /// the most common reason for everything downstream to go wrong.
    pub fn open_adapter<F: FnMut(Progress)>(&mut self, mut report: F) -> Result<()> {
        let mut controller = Controller::open_with_command_style(self.config.command_style).map_err(|e| match e {
            ControllerError::Transport(t) => ControllerError::Transport(t),
            other => other,
        })?;

        controller.initialize()?;

        let version = controller
            .local_version
            .as_ref()
            .map(|v| format!("Bluetooth {}", v.bluetooth_version()))
            .unwrap_or_else(|| "unknown".into());

        let address = controller
            .local_address
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());

        report(Progress::AdapterReady { version, address });
        self.controller = Some(controller);
        Ok(())
    }

    /// Scans for LE Audio devices, ignoring everything that is not one.
    pub fn scan<F: FnMut(Progress)>(&mut self, report: F) -> Result<Vec<DiscoveredDevice>> {
        self.scan_until(report, |_| false)
    }

    /// Scans, reporting each device the moment it is seen.
    ///
    /// `enough` is asked after every new device and ends the scan when it says
    /// so. Reconnecting uses it to stop as soon as the headphones it is looking
    /// for have answered, instead of sitting out the rest of the window for
    /// devices nobody asked about.
    ///
    /// Reporting used to happen after the whole window had elapsed, so nothing
    /// appeared for ten seconds and then everything appeared at once. The radio
    /// had usually seen the nearest device within the first fraction of a
    /// second; only the display waited.
    pub fn scan_until<F, E>(&mut self, mut report: F, mut enough: E) -> Result<Vec<DiscoveredDevice>>
    where
        F: FnMut(Progress),
        E: FnMut(&DiscoveredDevice) -> bool,
    {
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;

        let mut found: Vec<DiscoveredDevice> = Vec::new();
        let duration = self.config.scan_duration;

        // Everything that answers is kept, not only what advertises an LE Audio
        // service. Plenty of headphones publish PACS only once connected, or send
        // a shortened advertisement with no service list at all - filtering here
        // would make them look absent rather than merely unannounced.
        let announce = |device: &DiscoveredDevice, report: &mut F| {
            report(Progress::DeviceFound {
                name: device.name.clone().unwrap_or_else(|| "(bez jmena)".into()),
                address: device.address.to_string(),
                    address_type: device.address_type,
                rssi: device.rssi,
                le_audio: device.is_le_audio(),
            });
        };

        controller.scan(duration, |device| {
            // Advertising data can be split across reports. The first report
            // is often nameless and the later scan response carries the local
            // name; discarding every duplicate made the displayed name depend
            // on packet timing.
            if let Some(existing) = found.iter_mut().find(|d| d.address == device.address) {
                let was_nameless = existing.name.as_deref().unwrap_or("").trim().is_empty();
                let now_named = device.name.as_deref().is_some_and(|n| !n.trim().is_empty());

                if was_nameless && now_named {
                    existing.name = device.name.clone();
                }
                existing.rssi = existing.rssi.max(device.rssi);
                for uuid in &device.service_uuids {
                    if !existing.service_uuids.contains(uuid) {
                        existing.service_uuids.push(*uuid);
                    }
                }

                // A device first seen without a name was announced as
                // "(bez jmena)". Say it again now there is something to call
                // it, rather than leaving the entry anonymous until the next
                // scan.
                if was_nameless && now_named {
                    let named = existing.clone();
                    announce(&named, &mut report);
                    return !enough(&named);
                }
                return true;
            }

            found.push(device.clone());
            announce(device, &mut report);
            !enough(device)
        })?;

        // Ones that did announce LE Audio come first: they are the likely target,
        // and the caller picks by signal strength within each group.
        found.sort_by_key(|d| (!d.is_le_audio(), -(d.rssi as i32)));

        if found.is_empty() {
            return Err(SessionError::NoDeviceFound);
        }

        Ok(found)
    }

    /// User-facing scan that a newer command can supersede immediately.
    pub fn find_bond<S: Fn() -> bool>(&mut self, bond: &crate::bonding::Bond, should_stop: S) -> Result<Option<DiscoveredDevice>> {
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        let mut found = None;
        controller.scan_until(Duration::from_secs(3), |device| {
            if bond.matches(device.address, device.address_type) {
                found = Some(device.clone());
                true
            } else { false }
        }, should_stop)?;
        Ok(found)
    }

    /// User-facing scan that a newer command can supersede immediately.
    pub fn scan_interruptible<F, S>(&mut self, mut report: F, should_stop: S) -> Result<Vec<DiscoveredDevice>>
    where
        F: FnMut(Progress),
        S: Fn() -> bool,
    {
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        let duration = self.config.scan_duration;
        let mut found: Vec<DiscoveredDevice> = Vec::new();

        controller.scan_until(
            duration,
            |device| {
                if let Some(existing) = found.iter_mut().find(|known| known.address == device.address) {
                    if existing.name.as_deref().unwrap_or("").trim().is_empty()
                        && device.name.as_deref().is_some_and(|name| !name.trim().is_empty())
                    {
                        existing.name = device.name.clone();
                    }
                    existing.rssi = existing.rssi.max(device.rssi);
                } else {
                    found.push(device.clone());
                }
                report(Progress::DeviceFound {
                    name: device.name.clone().unwrap_or_else(|| "(bez jmena)".into()),
                    address: device.address.to_string(),
                    address_type: device.address_type,
                    rssi: device.rssi,
                    le_audio: device.is_le_audio(),
                });
                true
            },
            &should_stop,
        )?;

        found.sort_by_key(|device| (!device.is_le_audio(), -(device.rssi as i32)));
        if found.is_empty() && !should_stop() {
            return Err(SessionError::NoDeviceFound);
        }
        Ok(found)
    }

    /// Connects to a device and returns the connection handle.
    pub fn connect<F: FnMut(Progress)>(
        &mut self,
        device: &DiscoveredDevice,
        report: F,
    ) -> Result<u16> {
        self.connect_within(device, Duration::from_secs(15), report)
    }

    /// The same, with an explicit limit on how long to keep the radio trying.
    ///
    /// A connection attempt is not a request that fails and returns: the
    /// controller keeps listening for the peer to advertise until it is told to
    /// stop. That makes the timeout the interesting parameter rather than an
    /// implementation detail.
    ///
    /// Someone who has just pressed Connect deserves a long window - the
    /// headphones are in their hands. Automatic reconnect wants a short one and
    /// many of them: the controller then spends almost every moment listening,
    /// so the connection completes the instant the headphones come back, and
    /// the gap between attempts is where the retry interval actually lives.
    pub fn connect_within<F: FnMut(Progress)>(
        &mut self,
        device: &DiscoveredDevice,
        timeout: Duration,
        report: F,
    ) -> Result<u16> {
        self.connect_within_until(device, timeout, report, || false)
    }

    /// The same attempt, abandoned as soon as `should_stop` says so.
    ///
    /// The listening window is deliberately long, because a short one is why a
    /// reconnect misses headphones that are one second slow to advertise. That
    /// makes it the longest thing a person can be waiting on, so it has to be
    /// the thing they can interrupt.
    pub fn connect_within_until<F: FnMut(Progress), S: Fn() -> bool>(
        &mut self,
        device: &DiscoveredDevice,
        timeout: Duration,
        mut report: F,
        should_stop: S,
    ) -> Result<u16> {
        if should_stop() { return Err(SessionError::ConnectFailed); }
        crate::media::reset_link();
        if self.controller.as_ref().is_some_and(|controller| !controller.pump().alive()) {
            self.shutdown();
        }
        if self.controller.is_none() {
            self.open_adapter(&mut report)?;
        }
        let link_timeout = self.config.link_timeout;
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;

        // Applied per attempt, so changing it in the settings takes effect on
        // the next reconnect rather than on the next launch.
        controller.set_supervision_timeout(link_timeout);

        let handle = controller
            .connect_until(device.address, device.address_type, timeout, should_stop)?
            .ok_or(SessionError::ConnectFailed)?;

        report(Progress::Connected { handle });
        Ok(handle)
    }

    /// Runs LE Secure Connections pairing and turns on encryption.
    ///
    /// LE Audio characteristics sit behind encryption, so without this PACS stays
    /// unreadable no matter how correct the rest of the stack is.
    pub fn pair(
        &mut self,
        link: &mut Link,
        handle: u16,
        peer: &DiscoveredDevice,
    ) -> Result<[u8; 16]> {
        self.peer_identity = None;
        let local = self
            .controller
            .as_ref()
            .and_then(|c| c.local_address)
            .ok_or(SessionError::NoDeviceFound)?;

        let local_addr = smp::addressed(local.0, false);
        let peer_addr = smp::addressed(peer.address.0, peer.address_type != 0);

        let step = Duration::from_secs(10);
        let (mut pairing, request) = smp::Pairing::start(local_addr, peer_addr, 16);

        // Request -> Response
        let response = link.smp_exchange(&request, step)?;
        let public_key_pdu = pairing.handle_response(&response)?;
        let receive_identity = response[6] & 0x02 != 0;

        // Our public key -> theirs
        let peer_key = link.smp_exchange(&public_key_pdu, step)?;
        pairing.handle_public_key(&peer_key)?;

        // Their confirm arrives next, then we reveal our nonce.
        let confirm = link.smp_receive(step)?;
        let random_pdu = pairing.handle_confirm(&confirm)?;

        // Their nonce, checked against the confirm they committed to.
        let peer_random = link.smp_exchange(&random_pdu, step)?;
        let dhkey_check = pairing.handle_random(&peer_random)?;

        // Both sides prove they derived the same key.
        let peer_check = link.smp_exchange(&dhkey_check, step)?;
        let result = pairing.handle_dhkey_check(&peer_check)?;

        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        let encrypted =
            controller.enable_encryption(handle, &result.long_term_key, Duration::from_secs(10))?;

        if !encrypted {
            return Err(SessionError::EncryptionFailed);
        }

        crate::media::authorize();
        if receive_identity {
            let key = link.smp_receive(step)?;
            let address = link.smp_receive(step)?;
            self.peer_identity = Some(crate::privacy::PeerIdentity::from_pdus(&key, &address)?);
            crate::trace::note("peer identity received over encrypted SMP; IRK not logged");
        }
        Ok(result.long_term_key)
    }

    /// Restores encryption for a bond that has already completed LE Secure
    /// Connections. Reusing the LTK is both faster and what a paired peripheral
    /// expects; running the full pairing exchange on every reconnect can make
    /// the peer reject an otherwise valid known host.
    pub fn resume_encryption(&mut self, handle: u16, long_term_key: &[u8; 16]) -> Result<()> {
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        let encrypted = controller.enable_encryption(handle, long_term_key, Duration::from_secs(10))?;

        if !encrypted {
            return Err(SessionError::EncryptionFailed);
        }

        crate::media::authorize();
        Ok(())
    }

    /// Reads everything the device publishes about its audio capabilities.
    ///
    /// This is the answer Windows refuses to give: the actual LC3 configurations
    /// the headphones will accept.
    /// Applies a notification that may be the headset's availability changing.
    ///
    /// Returns true when this notification was the contexts characteristic, so
    /// the caller knows not to look for anything else in it.
    fn absorb_contexts(&mut self, notified: u16, value: &[u8]) -> bool {
        if self.contexts_handle != Some(notified) {
            return false;
        }

        let (sink, _) = crate::link::parse_context_pair(value);
        if let Some(contexts) = sink {
            self.available_contexts = Some(contexts);
            self.media_available = contexts & self.active_context != 0;
        }
        true
    }

    pub fn set_audio_context(&mut self, context: u16) {
        self.active_context = context;
        self.media_available = self.available_contexts.map_or(true, |available| available & context != 0);
    }

    /// Whether the headset is offering to play media to us at this moment.
    pub fn media_available(&self) -> bool {
        self.media_available
    }

    pub fn read_capabilities<F: FnMut(Progress)>(
        &mut self,
        link: &mut Link,
        mut report: F,
    ) -> Result<AudioCapabilities> {
        let capabilities = link.read_audio_capabilities()?;

        self.contexts_handle = capabilities.available_contexts_handle;
        self.available_contexts = capabilities.available_contexts;
        self.media_available = match capabilities.available_contexts {
            Some(contexts) => contexts & crate::bap::ascs::CONTEXT_MEDIA != 0,
            // Nothing published is not the same as nothing available. A device
            // that says nothing has to be assumed free, or it is never played to.
            None => true,
        };

        let summary = describe_capabilities(&capabilities);
        report(Progress::CapabilitiesRead { summary });

        Ok(capabilities)
    }

    /// Turns capabilities into a validated plan and checks it against hard limits.
    pub fn plan_stream<F: FnMut(Progress)>(
        &mut self,
        capabilities: &AudioCapabilities,
        mut report: F,
    ) -> Result<StreamPlan> {
        // Android's priority list, not one of our presets. The presets were
        // read out of a trace of the Windows driver and describe a different
        // stack; keeping them in this path meant a stream could come from
        // either source with nothing in the log saying which.
        let chosen = StreamPlan::google_choice(capabilities, self.config.prefer_single_cis);
        let plan = StreamPlan::build_google(
            capabilities,
            self.config.prefer_single_cis,
            None,
        )?;

        // Belt and braces: the plan already respects the device's own limits, but
        // these bounds apply to any device, whatever it claims to accept.
        safety::check_stream_parameters(
            plan.codec.octets_per_frame,
            plan.qos.sdu_interval_us,
            plan.qos.retransmission_number,
            plan.qos.max_transport_latency_ms,
        )
        .map_err(SessionError::UnsafeParameters)?;

        let mut summary = plan.describe();
        if let Some(chosen) = chosen {
            // How far down Android's list this landed. First place means the
            // headphones took what a phone would have asked for; anything else
            // means they turned that down, which is the difference between
            // working and working as well as this device can.
            summary.push_str(&format!(
                " [Google/Android LC3 {}{}]",
                chosen.setting.name,
                if chosen.rank == 0 {
                    String::new()
                } else {
                    format!(", {}. volba", chosen.rank + 1)
                }
            ));
        }

        report(Progress::StreamPlanned { summary });
        Ok(plan)
    }

    /// Sends the ASCS configuration, having first checked every write is allowed.
    pub fn configure_stream(
        &mut self,
        link: &mut Link,
        control_point_handle: u16,
        plan: &StreamPlan,
    ) -> Result<()> {
        // Approve the handle on both layers: the session tracks it for reporting,
        // the link enforces it on every write that leaves this process.
        self.write_policy.allow_ase_control_point(control_point_handle);
        link.allow_writes_to(control_point_handle);

        self.write_ascs(link, control_point_handle, &plan.ascs_sequence())
    }

    /// Sends a list of ASCS operations, each checked before it leaves.
    ///
    /// Split out so the caller can stop between Config Codec and Config QoS and
    /// read what the device said it prefers.
    pub fn write_ascs(
        &mut self,
        link: &mut Link,
        control_point: u16,
        operations: &[Vec<u8>],
    ) -> Result<()> {
        self.write_policy.allow_ase_control_point(control_point);
        link.allow_writes_to(control_point);

        for operation in operations {
            self.write_policy.check_write(control_point, operation)?;
            if let Some(op)=operation.first() {link.begin_ascs_operation(*op);}
            link.write_characteristic(control_point, operation)?;
            link.check_ascs_errors()?;
        }

        Ok(())
    }

    /// Hooks up the headphones' volume buttons, if they have any.
    ///
    /// Absence is not failure. A device with no Volume Control Service simply
    /// has no remote volume, and reporting that as an error would stop a stream
    /// that is otherwise perfectly fine.
    pub fn attach_volume_control(&mut self, link: &mut Link) -> Result<Option<String>> {
        let Some(handles) = link.discover_volume_control()? else {
            return Ok(None);
        };

        // Approve on both layers, exactly as the ASE control point is.
        self.write_policy.allow_volume_control_point(handles.control_point);
        self.write_policy.allow_subscription(handles.state_cccd);
        link.allow_volume_writes_to(handles.control_point);

        link.subscribe(handles.state_cccd)?;

        let state = link
            .read_volume_state(handles.state)?
            .unwrap_or(crate::vcs::VolumeState { setting: 0, muted: false, change_counter: 0 });

        let bridge = VolumeBridge {
            handles,
            state,
            system: crate::audio::SystemVolume::open_default_render().ok(),
        };

        let summary = bridge.describe();
        self.volume = Some(bridge);
        Ok(Some(summary))
    }

    /// Reads the headphones' battery levels and asks to be told when they change.
    ///
    /// Returns what was read, which is empty when the device publishes no
    /// battery information - a normal state, not a failure. Subscribing is best
    /// effort for the same reason: a device that reports a level but refuses
    /// notifications still gives a useful number, it just gives it once.
    pub fn attach_batteries(&mut self, link: &mut Link) -> Result<Vec<u8>> {
        self.batteries.clear();

        for mut handles in link.discover_batteries()? {
            let level = link.read_battery_level(handles.level).ok().flatten();

            if handles.notifies {
                self.write_policy.allow_subscription(handles.level_cccd);
                // A failed subscription must fall back to the sparse poll. Keeping
                // `notifies` true here would freeze the displayed level forever.
                handles.notifies = link.subscribe(handles.level_cccd).is_ok();
            }

            self.batteries.push((handles, level));
        }

        Ok(self.battery_levels())
    }

    /// The levels last seen, in the order the device publishes them.
    pub fn battery_levels(&self) -> Vec<u8> {
        self.batteries.iter().filter_map(|(_, level)| *level).collect()
    }

    /// Sets the headphones' volume from a Windows-style 0.0-1.0 level.
    pub fn set_volume(&mut self, link: &mut Link, level: f32) -> Result<()> {
        let Some(bridge) = self.volume.as_mut() else {
            return Ok(());
        };

        let setting = crate::vcs::VolumeState::setting_from_scalar(level);
        let pdu = crate::vcs::set_absolute(&bridge.state, setting);
        self.write_policy.check_write(bridge.handles.control_point, &pdu)?;
        link.write_characteristic(bridge.handles.control_point, &pdu)?;
        Ok(())
    }

    /// Creates the isochronous group and opens the data path.
    /// Tears the isochronous group down, so a retry starts from nothing.
    ///
    /// A CIG that was half created still occupies its id in the controller, and
    /// the next attempt to create the same id is refused or ignored. That turns
    /// one failure into every subsequent attempt failing, which reads as the
    /// headphones being broken rather than as leftover state on our side.
    /// Errors are swallowed on purpose: this runs on the failure path, and a
    /// second error here would replace the one worth reporting.
    /// Disconnects channels that did come up, so the group can be removed.
    ///
    /// A group with an established channel in it cannot be removed, and the ASE
    /// belonging to that channel is still busy - so the retry that follows finds
    /// the device unable to answer and times out. Tearing the survivors down is
    /// what makes starting over actually start over.
    /// Disconnects isochronous channels, taking their data paths down first.
    ///
    /// The order matters and the first half used to be missing entirely. A data
    /// path is a standing arrangement for one channel; while it exists the
    /// controller counts the channel as in use, and a group with a channel in
    /// use cannot be removed.
    pub fn release_cis(&mut self, handles: &[u16]) -> Vec<u16> {
        let mut pending = handles.to_vec();
        pending.sort_unstable();
        pending.dedup();

        // The handles are deliberately NOT forgotten here.
        //
        // They were, and that was wrong in the one case this whole mechanism
        // exists for. Disconnecting a channel fails when the ACL link is
        // already gone; the group then survives, and having forgotten the
        // handles there was nothing left to disconnect on the next attempt -
        // so the pre-clean was back to a bare group removal and the leftover
        // was refused all over again. They are cleared where the group is
        // actually removed, and only if that succeeded.
        if let Some(controller) = self.controller.as_mut() {
            for &handle in &pending {
                let _ = controller.command(&crate::hci::le_remove_iso_data_path(
                    handle,
                    crate::hci::ISO_PATH_BOTH,
                ));
            }

            // Disconnect every survivor first, then wait for their completion
            // events as one batch. BT600 acknowledges Disconnect immediately
            // but the Disconnection Complete from a half-established CIS can
            // arrive about two seconds later. Waiting 800 ms per handle raced
            // that event, attempted Remove CIG too early and made every retry
            // fail with Command Disallowed.
            let requested = pending.clone();
            for &handle in &requested {
                match controller.command(&crate::hci::disconnect(
                    handle,
                    crate::hci::REASON_REMOTE_USER_TERMINATED,
                )) {
                    Ok(_) => {}
                    Err(ControllerError::CommandFailed { status: 0x02, .. }) => {
                        // Unknown handle means it is already gone.
                        pending.retain(|candidate| *candidate != handle);
                    }
                    Err(_) => {}
                }
            }

            let deadline = Instant::now() + Duration::from_secs(3);
            while !pending.is_empty() && Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wanted = pending.clone();
                match controller.wait_for_event(remaining, |event| {
                    crate::hci::parse_disconnection_complete(event)
                        .map(|(closed, _)| wanted.contains(&closed))
                        .unwrap_or(false)
                }) {
                    Ok(Some(event)) => {
                        if let Some((closed, _)) = crate::hci::parse_disconnection_complete(&event) {
                            pending.retain(|candidate| *candidate != closed);
                        }
                    }
                    _ => break,
                }
            }
        }

        pending
    }

    /// Tears the isochronous group down: channels first, then the group.
    ///
    /// The order is not a preference, it is the only order that works. A
    /// controller refuses to remove a group while any of its channels is still
    /// established, and it equally refuses to configure a group that still
    /// exists. Removing the group without disconnecting the channels therefore
    /// fails silently and leaves the group behind non-configurable, and the
    /// next `LE Set CIG Parameters` is answered with "command disallowed" - a
    /// message about the request that names nothing about the leftover that
    /// actually caused it.
    ///
    /// `established` may be empty when nothing came up, which is the case a
    /// bare group removal is right for.
    pub fn release_isochronous(&mut self, plan: &StreamPlan, established: &[u16]) {
        // Include handles remembered from an earlier partial setup. The old
        // code only retained handles after a completely successful group, so
        // the exact failure case that needed cleanup most was forgotten.
        let mut known = self.last_cis_handles.clone();
        known.extend_from_slice(established);
        known.sort_unstable();
        known.dedup();
        let unresolved = self.release_cis(&known);

        if let Some(controller) = self.controller.as_mut() {
            // Now it can go. Worth knowing when it does not: a group that
            // survives here is the one the next connection will be refused
            // over, and until now that refusal arrived with nothing pointing
            // back to the teardown that caused it.
            match controller.command(&crate::hci::le_remove_cig(plan.cig_id)) {
                Ok(_) => {
                    // Gone for real, so there is nothing left to clean up
                    // before the next group is built.
                    self.last_cis_handles.clear();
                }
                Err(error) => {
                    // Keep the handles. The group survived, and the next
                    // attempt needs to know which channels are holding it.
                    self.last_cis_handles = if unresolved.is_empty() {
                        known
                    } else {
                        unresolved
                    };
                    crate::trace::note(&format!(
                        "CIG {:#04x} could not be removed: {error} - keeping {} channel handle(s) for the next attempt",
                        plan.cig_id,
                        self.last_cis_handles.len()
                    ));
                }
            }
        }
    }

    /// Teardown uses both GATT and HCI; either can observe the ACL going away.
    pub fn service_recovery_link(&mut self, link: &mut Link, handle: u16) -> Result<Option<u8>> {
        if !self.controller.as_ref().is_some_and(|controller| controller.pump().alive()) {
            return Err(SessionError::AdapterSilent);
        }
        for (notified, value) in link.collect_notifications(Duration::from_millis(50)) {
            if self.absorb_contexts(notified, &value) { continue; }
            if let Some(bridge) = self.volume.as_mut() {
                if notified == bridge.handles.state { bridge.absorb(&value); }
            }
            for (handles, level) in &mut self.batteries {
                if handles.level == notified { *level = crate::link::parse_battery_level(&value); }
            }
        }
        Ok(self.take_disconnection_reason(link, handle))
    }

    pub fn configure_acl_phy(&mut self, handle: u16, preference: u8) -> Result<String> {
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        controller.command(&hci::le_set_phy(handle, preference))?;
        let update = controller.wait_for_event(Duration::from_secs(2), |event|
            event.subevent() == Some(0x0c) && event.params.len() >= 6
                && u16::from_le_bytes([event.params[2], event.params[3]]) == handle)?;
        if let Some(event) = update { return Ok(hci::link_update_summary(&event).unwrap()); }
        let p = controller.command(&hci::command(hci::op::LE_READ_PHY, &handle.to_le_bytes()))?;
        if p.len() >= 5 && u16::from_le_bytes([p[1],p[2]]) == handle {
            Ok(format!("ACL PHY currently TX {}, RX {} (1=1M, 2=2M); requested mask {preference:#04x}", p[3],p[4]))
        } else { Ok("ACL PHY preference sent; actual PHY unavailable".into()) }
    }

    /// Teardown uses both GATT and HCI; either can observe the ACL going away.
    pub fn take_disconnection_reason(&mut self, link: &Link, handle: u16) -> Option<u8> {
        link.disconnected_reason().or_else(|| {
            self.controller.as_mut().and_then(|controller| controller.take_disconnection_reason(handle))
        })
    }

    /// Failed setup owns a fixed CIG even before it has active CISes.
    pub fn release_prepared_group(&mut self) {
        let handles = self.last_cis_handles.clone();
        let pending = self.release_cis(&handles);
        if let Some(controller) = self.controller.as_mut() {
            match controller.command(&hci::le_remove_cig(0x01)) {
                Ok(_) | Err(ControllerError::CommandFailed { status: 0x02, .. }) => self.last_cis_handles.clear(),
                Err(error) => {
                    if !pending.is_empty() { self.last_cis_handles = pending; }
                    crate::trace::note(&format!("CIG cleanup pending: {error}"));
                }
            }
        }
    }

    /// Builds the group and brings the channels up, retrying on the radio alone.
    ///
    /// A failed channel is very often just timing: the same request a moment
    /// later succeeds. Retrying here costs nothing but a round trip, and it
    /// touches only the controller - no GATT, no reconfiguration, nothing the
    /// headphones have to answer. That matters because after a failed channel
    /// this device stops answering ATT for a while, so any recovery that needs
    /// to talk to it times out and turns one bad attempt into a dead session.
    pub fn establish_isochronous(&mut self, plan: &StreamPlan, acl_handle: u16) -> Result<CisOutcome> {
        const RADIO_ATTEMPTS: u32 = 3;

        let mut last = Err(SessionError::NoCisEstablished);
        let mut survivors: Vec<u16> = Vec::new();

        for attempt in 1..=RADIO_ATTEMPTS {
            survivors.clear();
            let outcome = self.establish_once(plan, acl_handle);

            match &outcome {
                Ok(done) if done.complete() => {
                    // Remember what came up. The next attempt at a group has to
                    // disconnect these before it can remove the old one, and by
                    // then this plan is gone.
                    self.last_cis_handles = done.established.clone();
                    return outcome;
                }
                Ok(done) => {
                    // Keep every partial handle until release_isochronous has
                    // removed the data paths, disconnected the channels and
                    // confirmed that the group itself is gone. Releasing here
                    // and then releasing again below used two short waits and
                    // still raced the single late completion event.
                    survivors = done.established.clone();
                }
                Err(_) => {}
            }

            // How long to leave it before asking again depends on who said no.
            //
            // A channel that simply did not come up is usually timing, and two
            // hundred milliseconds is enough. A channel the peer refused for
            // want of resources is the peer saying it is not ready, and asking
            // again immediately is how one refusal becomes three - this device
            // stops answering ATT at all for several seconds after refusing,
            // so the attempts that follow fail for a different reason and hide
            // the first one.
            // 0x14 is "low resources", 0x13 is "the remote user ended it".
            // Both are the peer declining, as opposed to a channel that never
            // answered.
            let peer_refused = match &outcome {
                Ok(done) => done
                    .failed
                    .iter()
                    .any(|&(_, status)| status == 0x13 || status == 0x14),
                Err(SessionError::CisFailed { status, .. }) => {
                    *status == 0x13 || *status == 0x14
                }
                Err(_) => false,
            };

            last = outcome;

            if attempt < RADIO_ATTEMPTS {
                let leftovers = std::mem::take(&mut survivors);
                self.release_isochronous(plan, &leftovers);
                std::thread::sleep(if peer_refused {
                    Duration::from_millis(1_500)
                } else {
                    Duration::from_millis(200)
                });
            }
        }

        last
    }

    fn establish_once(&mut self, plan: &StreamPlan, acl_handle: u16) -> Result<CisOutcome> {
        let handles = self.prepare_isochronous(plan, acl_handle)?;
        self.establish_prepared(plan, acl_handle, &handles)
    }

    /// Validate controller QoS before sending Config QoS to the headphones.
    pub fn prepare_isochronous(&mut self, plan: &StreamPlan, acl_handle: u16) -> Result<Vec<u16>> {
        // Clear anything left from a previous stream before asking for a new
        // group. A CIG that still exists makes LE Set CIG Parameters answer
        // "command disallowed".
        //
        // This used to be a bare group removal, and the comment above it already
        // said why that could not work: a group whose channels are still
        // established cannot be removed either. So in the one case the clean-up
        // existed for - a stream that ended without an orderly teardown - it did
        // nothing, the group survived, and the next attempt was refused over it.
        // Worse, the refusal cascaded: the failed attempt was retried, the retry
        // re-paired, and the bond was lost. One leftover, three symptoms.
        //
        // Data paths first, then the channels, then the group. That is the only
        // order the controller accepts.
        let leftovers = std::mem::take(&mut self.last_cis_handles);
        let mut unresolved = Vec::new();
        if !leftovers.is_empty() {
            crate::trace::note(&format!(
                "releasing {} isochronous channel(s) left by the previous stream",
                leftovers.len()
            ));
            unresolved = self.release_cis(&leftovers);
        }

        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;

        // Unknown CIG is the clean first-run state. Any other refusal means the
        // old group is still live; do not immediately ask to overwrite it and
        // turn the cleanup error into a misleading parameter error.
        match controller.command(&hci::le_remove_cig(plan.cig_id)) {
            Ok(_) | Err(ControllerError::CommandFailed { status: 0x02, .. }) => {
                self.last_cis_handles.clear();
            }
            Err(error) => {
                self.last_cis_handles = if unresolved.is_empty() {
                    leftovers
                } else {
                    unresolved
                };
                return Err(SessionError::IsoPath(format!(
                    "previous CIG {:#04x} could not be removed after disconnecting its channels: {error}",
                    plan.cig_id
                )));
            }
        }

        // The headphones must receive exactly the QoS accepted by the controller.
        // Never relax only the controller half after ASCS has been configured.
        let command = plan.cig_command();
        safety::check_hci_command(&command)?;
        let response = controller.command(&command).map_err(|error| {
            SessionError::IsoPath(format!(
                "controller rejected the requested QoS ({} us, {} B, RTN {}, {} ms): {error}",
                plan.qos.sdu_interval_us, plan.qos.max_sdu,
                plan.qos.retransmission_number, plan.qos.max_transport_latency_ms
            ))
        })?;

        // Return parameters: status, CIG id, CIS count, then one handle each.
        let mut cis_handles = Vec::new();
        if response.len() >= 3 {
            let count = response[2] as usize;
            for index in 0..count {
                let offset = 3 + index * 2;
                if offset + 2 <= response.len() {
                    cis_handles.push(u16::from_le_bytes([response[offset], response[offset + 1]]));
                }
            }
        }

        // Printed before the first Create CIS, because "unknown connection
        // identifier" from that command names neither handle it disliked, and
        // the two candidates - a CIS handle and the ACL handle - need very
        // different fixes.
        crate::trace::note(&format!(
            "CIG {:#04x}: channels {:?}, ACL handle {acl_handle:#06x}",
            plan.cig_id, cis_handles
        ));

        if cis_handles.is_empty() {
            return Err(SessionError::NoCisEstablished);
        }

        let expected = plan.ase_ids.len().max(usize::from(plan.microphone.is_some())).max(1);
        self.last_cis_handles = cis_handles.clone();
        let mut unique = cis_handles.clone();
        unique.sort_unstable();
        unique.dedup();
        if response.first() != Some(&0) || response.get(1) != Some(&plan.cig_id)
            || cis_handles.len() != expected || unique.len() != expected
            || response.len() != 3 + expected * 2
        {
            return Err(SessionError::IsoPath("controller returned an incomplete or invalid CIS mapping".into()));
        }
        Ok(cis_handles)
    }

    /// Opens the already configured group, without changing its QoS or retrying
    /// one half of an ASCS transaction. The caller must rebuild the whole stream
    /// after a failed attempt.
    pub fn establish_prepared(
        &mut self, plan: &StreamPlan, acl_handle: u16, cis_handles: &[u16],
    ) -> Result<CisOutcome> {
        if cis_handles.is_empty() || cis_handles != self.last_cis_handles.as_slice() {
            return Err(SessionError::IsoPath("CIS group was not prepared for this attempt".into()));
        }
        let controller = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?;
        // LE Create CIS answers with a Command Status, which only says the
        // request was accepted. Each channel reports separately afterwards, and
        // a channel that never reports is precisely the failure this stack
        // exists to handle - so wait for the proof rather than assuming it.
        //
        // Asked for once, deliberately. A channel that reports a failure has had
        // its handle released by the controller, so asking again with the same
        // handle is answered with "unknown connection identifier" - and an
        // earlier version of this function then returned that error, throwing
        // away the channel that had just come up successfully. That is exactly
        // the behaviour this project exists to replace, reproduced by the code
        // meant to avoid it. Recovering a failed channel means rebuilding the
        // whole group, which would drop the working one too; keeping what works
        // is worth more.
        let mut outcome = CisOutcome::default();

        let pairs: Vec<(u16, u16)> = cis_handles.iter().map(|&cis| (cis, acl_handle)).collect();
        let create = hci::le_create_cis(&pairs);
        safety::check_hci_command(&create)?;

        if let Err(e) = controller.command(&create) {
            // "Unknown connection identifier" names neither handle it disliked,
            // and the two possibilities need opposite fixes: a dead ACL means
            // the link went away and the whole connection has to be rebuilt,
            // while dead CIS handles mean the group we just created is not the
            // group the controller thinks it has.
            //
            // Read RSSI is the cheapest question that distinguishes them. It
            // takes an ACL handle and nothing else, so if it answers, the ACL is
            // alive and the CIS handles are the problem.
            let acl_alive = controller
                .command(&hci::read_rssi(acl_handle))
                .is_ok();

            let blame = if acl_alive {
                format!(
                    "the ACL link {acl_handle:#06x} is still up, so the controller is \
                     refusing the isochronous handles {cis_handles:?} it handed out itself"
                )
            } else {
                format!("the ACL link {acl_handle:#06x} is gone")
            };

            return Err(SessionError::IsoPath(format!(
                "{e}; {blame}. CIG {:#04x}, requested CIS handles {cis_handles:?}",
                plan.cig_id,
            )));
        }

        // All CISes are created by one command, so they share one establishment
        // deadline. Waiting the full timeout once per missing channel made a
        // stereo failure take twice as long as a mono one before cleanup could
        // even begin.
        let cis_deadline = Instant::now() + CIS_ESTABLISH_TIMEOUT;
        while outcome.established.len() + outcome.failed.len() < cis_handles.len() {
            // The peer ending the ACL link counts as an answer too, and it is
            // the one that used to be missed: waiting eight seconds for a
            // channel that will never come is exactly when a headset gives up
            // on the connection, and carrying on afterwards means building the
            // next attempt on a handle the controller has already forgotten.
            let remaining = cis_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut acl_gone = None;
            let event = controller.wait_for_event(remaining, |e| {
                if let Some((closed, reason)) = hci::parse_disconnection_complete(e) {
                    if closed == acl_handle {
                        acl_gone = Some(reason);
                        return true;
                    }
                }
                hci::parse_cis_established(e).is_some_and(|(_, handle)| {
                    cis_handles.contains(&handle)
                        && !outcome.established.contains(&handle)
                        && !outcome.failed.iter().any(|(seen, _)| *seen == handle)
                })
            })?;

            if let Some(reason) = acl_gone {
                return Err(SessionError::IsoPath(format!(
                    "the headphones ended the connection while the channels were \
                     being established: {} (code {reason:#04x})",
                    crate::hci::disconnect_reason(reason)
                )));
            }

            match event.and_then(|e| hci::parse_cis_established(&e)) {
                Some((0x00, handle)) => outcome.established.push(handle),
                Some((status, handle)) => outcome.failed.push((handle, status)),
                None => break,
            }
        }

        // Back into the order the group defines, not the order the controller
        // happened to report them in. Channel n of the audio goes to CIS n, and
        // the establishment events arrive whenever each channel manages it - so
        // sorting by arrival silently swaps left and right whenever the second
        // channel comes up first.
        outcome
            .established
            .sort_by_key(|handle| cis_handles.iter().position(|h| h == handle).unwrap_or(usize::MAX));

        // Anything that never reported at all counts as failed, so the caller is
        // never told a channel is fine because the controller went quiet.
        for &handle in cis_handles {
            let seen = outcome.established.contains(&handle)
                || outcome.failed.iter().any(|(h, _)| *h == handle);
            if !seen {
                outcome.failed.push((handle, 0xFF));
            }
        }

        if outcome.established.is_empty() {
            let worst = outcome.failed.first().copied();
            return Err(match worst {
                Some((handle, status)) => SessionError::CisFailed { handle, status },
                None => SessionError::NoCisEstablished,
            });
        }

        if !outcome.complete() {
            return Ok(outcome);
        }
        let established = outcome.established.clone();

        // Relaxed only now, with the group built and every ASE configured. Doing
        // it earlier would slow down the very GATT exchange that sets the stream
        // up, which is the opposite of what anyone wants.
        if self.config.idle_link_latency > 0 {
            let latency = self.config.idle_link_latency;
            let interval = 0x0018u16; // 30 ms
            let timeout_units = (self.config.link_timeout.as_millis() / 10) as u16;

            // The link must not be able to time out while the headphones are
            // legitimately asleep. Six times the longest gap they may take is
            // the specification's own margin, and the configured timeout is used
            // whenever it is already longer than that.
            let needed = interval
                .saturating_mul(latency.saturating_add(1))
                .saturating_mul(5)
                / 4;
            let timeout = hci::clamp_supervision(timeout_units.max(needed));

            let update = hci::le_connection_update(acl_handle, interval, interval, latency, timeout);
            match controller.command(&update) {
                Ok(_) => crate::trace::note(&format!(
                    "link relaxed: the headphones may skip up to {latency} connection events,                      timeout {} ms",
                    timeout as u32 * 10
                )),
                // Not fatal, and not worth failing a working stream over. The
                // link simply stays as it was, which is what it did before this
                // existed.
                Err(error) => crate::trace::note(&format!(
                    "the controller refused to relax the link ({error}); leaving it as negotiated"
                )),
            }
        }

        // Transparent data path: the controller forwards our LC3 bytes untouched.
        // Only for channels that actually came up.
        for (index, &handle) in established.iter().enumerate() {
            if plan.playback_enabled && index < plan.ase_ids.len() {
                let setup = hci::le_setup_iso_data_path(handle, hci::ISO_PATH_INPUT, 0);
                safety::check_hci_command(&setup)?;
                controller.command(&setup)?;
            }
            if plan
                .microphone
                .as_ref()
                .is_some_and(|microphone| microphone.cis_id as usize == index)
            {
                let setup = hci::le_setup_iso_data_path(handle, hci::ISO_PATH_OUTPUT, 0);
                safety::check_hci_command(&setup)?;
                controller.command(&setup)?;
            }
        }

        Ok(outcome)
    }

    /// The audio loop: capture, encode, send, one frame per SDU interval.
    ///
    /// Runs until `should_stop` returns true or the device goes away.
    pub fn run_audio<F, S>(
        &mut self,
        plan: &StreamPlan,
        cis_handles: &[u16],
        acl_handle: Option<u16>,
        mut report: F,
        mut should_stop: S,
    ) -> Result<()>
    where
        F: FnMut(Progress),
        S: FnMut() -> bool,
    {
        // MMCSS protects the capture/encode/send deadline from ordinary
        // background CPU work. It is scoped to this loop and reverts on every
        // exit path, including errors and disconnects.
        let mut power=crate::radio::Power::default();
        let mut acl_tx_phy = None;
        if let (Some(controller),Some(acl))=(self.controller.as_mut(),acl_handle) {
            if let Ok(p)=controller.command(&crate::radio::range_command()) {
                if let Some((min,max))=crate::radio::range(&p) {power.min=Some(min);power.adapter_max=Some(max);}
            }
            if let Ok(p) = controller.command(&hci::command(hci::op::LE_READ_PHY, &acl.to_le_bytes())) {
                if p.len() >= 5 && u16::from_le_bytes([p[1],p[2]]) == acl && matches!(p[3], 1 | 2) {
                    acl_tx_phy = Some(p[3]); power.phy = p[3];
                }
            }
            if let Some(phy) = acl_tx_phy {
                if let Ok(p)=controller.command(&crate::radio::connection_command(acl,phy)) {
                    if let Some((current,max))=crate::radio::connection(&p,acl,phy) {
                        power.current=current;power.connection_max=max;power.available=true;
                    }
                }
            }
        }
        report(Progress::RadioPower(power.clone()));
        let _audio_priority = crate::audio::AudioThreadPriority::enter();

        let device = crate::audio::find_cable_device(self.config.audio_device.as_deref())?;
        let sample_rate = plan.codec.sampling_frequency.hz().unwrap_or(48_000);

        let capture_device = device.name.clone();
        let (mut capture, mut wake_frame) = match self.resume_capture.take() {
            Some((id, rate, capture, frame)) if id == device.id && rate == sample_rate =>
                (capture, Some(frame)),
            _ => (AudioCapture::open(&device.id, sample_rate)?, None),
        };

        report(Progress::CaptureReady {
            device: capture_device,
            format: capture.describe(),
        });

        // With one CIS the codec itself carries both channels. With two, the
        // configuration describes a single channel per CIS, but the capture side
        // still hands over stereo - so the encoder needs both channels either
        // way, and only the routing differs.
        let dual = plan.topology == crate::stream::Topology::DualCis;
        let mut encoder = if dual {
            AudioEncoder::stereo_pair(plan.codec)
        } else {
            AudioEncoder::new(plan.codec)
        };

        // One outstanding battery read at a time, matched to its answer by hand:
        // an ATT read response carries only the value, never the handle it
        // belongs to.
        let mut pending_battery: Option<u16> = None;
        let mut battery_cursor = 0;
        let mut battery_asked_at = Instant::now();

        let microphone_handle = plan
            .microphone
            .as_ref()
            .and_then(|microphone| cis_handles.get(microphone.cis_id as usize).copied());
        let mut microphone_decoder = plan
            .microphone
            .as_ref()
            .map(|microphone| AudioDecoder::new(microphone.codec));
        let microphone_rate = plan
            .microphone
            .as_ref()
            .and_then(|microphone| microphone.codec.sampling_frequency.hz())
            .unwrap_or(32_000);
        let mut microphone_render = if plan.microphone.is_some() {
            if let Some(target) = self.config.microphone_target.as_deref() {
                let target = crate::audio::find_cable_render_device(Some(target))?;
                let render = AudioRender::open(&target.id)?;
                crate::trace::note(&format!(
                    "mikrofon -> {} ({})",
                    target.name,
                    render.describe()
                ));
                if same_virtual_cable(&device.name, &target.name) {
                    crate::trace::note(
                        "WARNING: playback and microphone use the same VB-CABLE; use a second VB-CABLE A/B for an isolated input path",
                    );
                }
                Some(render)
            } else {
                None
            }
        } else {
            None
        };
        // Monitoring endpoints are opened lazily. With monitoring off (the
        // default), Windows is not asked for any microphone at all. Toggling or
        // changing the source while music plays swaps this capture object on
        // the next frame and does not touch the Bluetooth stream.
        let mut monitor_selection: Option<String> = None;
        let mut monitor_capture: Option<AudioCapture> = None;
        let mut monitor_samples: VecDeque<i16> = VecDeque::new();

        if cis_handles.is_empty() {
            return Err(SessionError::NoCisEstablished);
        }
        let swap_channels = self.config.swap_channels.clone();
        let live_audio = self.config.live_audio.clone();

        let buffer = self.controller.as_mut().ok_or(SessionError::NoDeviceFound)?
            .command(&hci::command(hci::op::LE_READ_BUFFER_SIZE_V2, &[]))?;
        if buffer.len() < 7 || buffer[0] != 0 {
            return Err(SessionError::IsoPath("cannot read controller ISO buffer limits".into()));
        }
        let iso_payload_limit = u16::from_le_bytes([buffer[4], buffer[5]]);
        let iso_capacity = buffer[6] as u16;
        if iso_capacity < cis_handles.len() as u16 || plan.sdu_size() > iso_payload_limit {
            return Err(SessionError::IsoPath(format!(
                "controller ISO buffers cannot carry this stereo frame: {} packets of {} bytes available",
                iso_capacity, iso_payload_limit)));
        }
        let mut credits = crate::iso_flow::IsoCredits::new(iso_capacity, cis_handles);
        let mut backpressure_frames = 0u64;
        crate::trace::note(&format!("ISO flow control: {iso_capacity} packets, {iso_payload_limit} bytes per packet"));

        let transport = {
            let controller = self.controller.as_ref().ok_or(SessionError::NoDeviceFound)?;
            controller.transport().clone()
        };

        // HCI ISO data leaves over the bulk endpoint, alongside ACL - see
        // `UsbTransport::send_iso` for why the isochronous one is the wrong
        // pipe despite its name.
        crate::trace::note("ISO through the bulk endpoint together with ACL");

        // Watched during playback: once the peer is gone there is nothing to
        // play to, and counting frames into a dead link only hides that.
        let pump = {
            let controller = self.controller.as_ref().ok_or(SessionError::NoDeviceFound)?;
            controller.pump()
        };

        // Playback is the longest-running step, and nothing else reads ACL while
        // it runs. The peer sends its connection parameter request unprompted
        // and then waits a minute for the answer before dropping the link, so
        // this loop has to keep listening even though it has nothing to ask for.
        let mut acl = crate::att::AclReassembler::new();

        // Full level from the first sample would be a transient in someone's ears.
        let mut soft_start =
            crate::safety::SoftStart::over(300, plan.qos.sdu_interval_us);

        // A microphone session must stay live even when PC playback is silent.
        // Silence release tears down the entire ISO group, including its source.
        let idle_timeout = silence_release_timeout(self.config.idle_timeout, plan.microphone.is_some());
        let mut silent_since: Option<Instant> = None;

        let clock = crate::audio::PreciseTimer::new();

        // Let the capture build a small head start before the cadence begins.
        //
        // Without this the first frames are requested before Windows has
        // produced any, the loop takes the underrun path immediately, and the
        // stream starts out of step with its own deadline - which it then
        // spends the next few seconds recovering from, audibly.
        let prime_deadline = Instant::now() + Duration::from_millis(120);
        while Instant::now() < prime_deadline
            && capture.backlog() < encoder.samples_per_frame() * 4
        {
            clock.sleep(Duration::from_millis(5));
            if should_stop() { return Ok(()); }
            capture.pump()?;
        }


        let frame_interval = Duration::from_micros(plan.qos.sdu_interval_us as u64);

        // Every wait in this loop goes through it. An ordinary sleep rounds up
        // to the system tick and turns a 7.5 ms cadence into an irregular one.

        let mut next_deadline = Instant::now();
        let mut frames_sent: u64 = 0;
        let mut iso_sent: u64 = 0;
        let mut delivered = vec![0u64; cis_handles.len()];
        let mut iso_failed: u64 = 0;
        // Intervals where Windows had produced no audio in time. A handful at
        // startup is normal; a steady stream of them is the PC failing to keep
        // up, which sounds exactly like a radio problem and is not one.
        let mut underruns: u64 = 0;
        // Consecutive frames the screening refused. A single refusal is muted
        // and forgotten; only an unbroken run of them means the audio source
        // itself has gone wrong, and the run has to be long enough that no
        // transient can reach it.
        // When the controller last confirmed it had sent something.
        //
        // The second half of noticing a stream that has died quietly. A dead
        // reader thread is one way for confirmations to stop; there is no reason
        // to assume it is the only one, and the symptom is what matters: audio
        // encoded every 7.5 ms that nothing acknowledges is audio nobody is
        // hearing. Judged only while actually transmitting, because during an
        // idle pause there is correctly nothing to confirm.
        let mut last_delivery = vec![Instant::now(); cis_handles.len()];
        let mut rejected_run: u32 = 0;
        let mut rejected_total: u64 = 0;
        let mut last_report = Instant::now();

        // One diagnostic question at a time, asked in turn.
        //
        // The controller accepts a limited number of outstanding commands - one,
        // on the adapters this runs on - so two independent pollers each with
        // their own "pending" flag can and do overlap, and the second command is
        // simply dropped. Cycling through them from a single slot keeps every
        // answer, and asking every 400 ms still refreshes each channel about
        // once a second.
        let mut last_diagnostic = Instant::now() - Duration::from_secs(2);
        let mut diagnostic_pending = false;
        let mut diagnostic_turn: usize = 0;
        let mut latest_rssi: Option<i8> = None;
        let mut quality: Vec<crate::hci::IsoLinkQuality> = Vec::new();

        loop {
            if should_stop() {
                report(Progress::Stopped { reason: "zastaveno uzivatelem".into() });
                return Ok(());
            }

            // A reader that has given up means no event will ever arrive again,
            // including the one that says the link has ended. Encoding on would
            // be filling a connection nobody can confirm still exists - which is
            // exactly the failure this ends: audio that simply stops, a frame
            // counter still climbing, a delivered counter frozen, and nothing
            // anywhere saying why. Reported as an error so the reconnect path
            // runs, because that is the only thing that can put it right.
            if !pump.alive() {
                return Err(SessionError::AdapterSilent);
            }

            // Consumed, not handed back. Nothing else reads events while audio
            // is running - the GATT link is idle and every step that waited for
            // one has finished - so putting them back means finding the same
            // event again on the next frame, and the one after that.
            //
            // An earlier version did exactly that. It counted every completion
            // report once per frame for the rest of the stream, so the delivered
            // counts ran into the millions after two seconds; the held queue
            // grew without bound because nothing ever left it; and each frame
            // spent longer than the last re-reading events it had already seen.
            // A leak of memory and of time, hidden inside a diagnostic.
            let mut disconnects = AudioDisconnects::default();
            while let Some(event) = pump.try_recv_event() {
                if event.subevent() == Some(0x0c) && event.params.len() >= 6 && event.params[1] == 0
                    && Some(u16::from_le_bytes([event.params[2], event.params[3]])) == acl_handle {
                    acl_tx_phy = matches!(event.params[4], 1 | 2).then_some(event.params[4]);
                    power.phy = acl_tx_phy.unwrap_or(0);
                    power.current = None; power.connection_max = None;
                }
                if let Some(summary) = hci::link_update_summary(&event) {
                    report(Progress::LinkState { summary });
                }
                if let Some((opcode, params)) = event.command_complete() {
                    if opcode == crate::radio::READ_CONNECTION {
                        diagnostic_pending=false;
                        if let Some((current,max))=acl_handle.zip(acl_tx_phy).and_then(|(h,phy)|crate::radio::connection(params,h,phy)) {
                            power.current=current;power.connection_max=max;
                        } else {power.current=None;power.connection_max=None;power.available=false;}
                        report(Progress::RadioPower(power.clone()));
                    }
                    if opcode == hci::op::READ_RSSI {
                        diagnostic_pending = false;
                        // status(1), connection_handle(2), rssi(1)
                        if params.len() >= 4 && params[0] == 0 {
                            let response_handle = u16::from_le_bytes([params[1], params[2]]);
                            if Some(response_handle) == acl_handle {
                                latest_rssi = (params[3] != 127).then_some(params[3] as i8);
                            }
                        }
                    }

                    if opcode == hci::op::LE_READ_ISO_LINK_QUALITY {
                        diagnostic_pending = false;
                        if let Some(reading) = hci::parse_iso_link_quality(params) {
                            match quality.iter_mut().find(|q| q.handle == reading.handle) {
                                Some(existing) => *existing = reading,
                                None => quality.push(reading),
                            }
                        }
                    }
                }
                if let Some((handle, reason)) = hci::parse_disconnection_complete(&event) {
                    disconnects.observe(acl_handle, cis_handles, handle, reason);
                }

                // Proof from the controller that each stream is really going
                // out. Two channels that both accept writes look identical from
                // here; one of them quietly not transmitting does not.
                for (handle, count) in hci::parse_number_of_completed_packets(&event) {
                    if let Some(slot) = cis_handles.iter().position(|&h| h == handle) {
                        let completed = credits.complete(handle, count);
                        delivered[slot] += completed as u64;
                        if completed > 0 {
                            last_delivery[slot] = Instant::now();
                        }
                    }
                }
            }

            if let Some(reason) = disconnects.acl {
                report(Progress::Disconnected { reason });
                return Ok(());
            }
            if let Some((handle, reason)) = disconnects.cis {
                return Err(SessionError::AudioDisconnected { handle, reason });
            }

            let live = live_audio
                .read()
                .map(|value| value.clone())
                .unwrap_or_default();

            // This loop always transmits. Silence release exits into wait_for_sound,
            // where diagnostics are not polled.
            let metrics = live.metrics_override.unwrap_or(self.config.metrics);
            let diagnostic_slots = match metrics {
                MetricsLevel::Off => 0,
                MetricsLevel::Signal => 1,
                MetricsLevel::Full => 2 + cis_handles.len(),
            };

            if diagnostic_pending && last_diagnostic.elapsed() >= Duration::from_secs(2) {
                diagnostic_pending = false;
            }
            if diagnostic_slots > 0
                && !diagnostic_pending
                && last_diagnostic.elapsed() >= Duration::from_millis(400)
            {
                // Signal strength, then each isochronous channel, then round
                // again. Every question is about the same connection, so there
                // is nothing to gain from asking any of them more often than the
                // others.
                let slot = diagnostic_turn % diagnostic_slots;
                diagnostic_turn = diagnostic_turn.wrapping_add(1);

                let command = if slot == 0 {
                    acl_handle.map(hci::read_rssi)
                } else if slot==cis_handles.len()+1 {
                    acl_handle.zip(acl_tx_phy).map(|(h,phy)|crate::radio::connection_command(h,phy))
                } else {
                    cis_handles
                        .get(slot - 1)
                        .copied()
                        .map(hci::le_read_iso_link_quality)
                };

                if let Some(command) = command {
                    if safety::check_hci_command(&command).is_ok()
                        && transport.send_command(&command).is_ok()
                    {
                        diagnostic_pending = true;
                    }
                }
                last_diagnostic = Instant::now();
            }


            let wanted_monitor = if live.monitor_enabled {
                Some(live.monitor_source.clone())
            } else {
                None
            };
            if wanted_monitor != monitor_selection {
                monitor_capture = None;
                monitor_samples.clear();
                monitor_selection = wanted_monitor.clone();
                if let Some(selection) = wanted_monitor.as_deref().filter(|value| *value != "headset") {
                    match crate::audio::find_capture_device(selection).and_then(|source| {
                        AudioCapture::open_microphone(&source.id).map(|capture| (source, capture))
                    }) {
                        Ok((source, capture)) => {
                            crate::trace::note(&format!(
                                "odposlech z PC: {} ({})",
                                source.name,
                                capture.describe()
                            ));
                            monitor_capture = Some(capture);
                        }
                        Err(error) => crate::trace::note(&format!(
                            "monitoring could not be enabled ({error}); playback continues"
                        )),
                    }
                }
            }

            // Asked for at most once per pass, and only when something actually
            // wants an answer. A read costs one small packet each way on a link
            // that is already carrying audio, so the honest cost is tiny - but
            // it is not nothing, which is why nothing here happens on its own
            // unless the user turned the poll on.
            if pending_battery.is_some() && battery_asked_at.elapsed() >= Duration::from_secs(30) {
                return Err(SessionError::BatteryReadTimeout);
            }
            if let Some(handle) = acl_handle {
                let asked_for = self
                    .config
                    .battery_refresh
                    .swap(false, Ordering::Relaxed);
                // Notifications cost no host-initiated radio round trip. Poll only
                // as a compatibility fallback for devices which do not expose
                // them, or whose CCCD subscription actually failed.
                let needs_poll = self.batteries.iter().any(|(handles, _)| !handles.notifies);
                let due = needs_poll
                    && live.battery_poll_override.unwrap_or(self.config.battery_poll)
                        .is_some_and(|every| battery_asked_at.elapsed() >= every);

                if asked_for && pending_battery.is_some() {
                    self.config.battery_refresh.store(true, Ordering::Relaxed);
                }
                if pending_battery.is_none() && (asked_for || due) {
                    let candidate = next_battery_index(&self.batteries, battery_cursor, asked_for);
                    if let Some(index) = candidate {
                        let handles = self.batteries[index].0;
                        let pdu = crate::att::read_request(handles.level);
                        let packets = crate::att::build_acl_packets(
                            handle,
                            crate::att::cid::ATT,
                            &pdu,
                            27,
                        );
                        let sent = packets
                            .iter()
                            .all(|packet| transport.send_acl(packet).is_ok());
                        if sent {
                            battery_cursor = (index + 1) % self.batteries.len();
                            pending_battery = Some(handles.level);
                            battery_asked_at = Instant::now();
                            report(Progress::BatteryAsked {
                                reason: if asked_for { "asked for" } else { "scheduled" },
                            });
                        }
                    }
                }
            }

            if let Some(handle) = acl_handle {
                while let Ok(raw) = pump.recv_acl(Duration::ZERO) {
                    if let Some(iso) = hci::parse_iso_data_packet(&raw) {
                        if Some(iso.handle) == microphone_handle {
                            if let Some(decoder) = microphone_decoder.as_mut() {
                                match decoder.decode(iso.payload) {
                                    Ok(decoded) => {
                                        if let Some(render) = microphone_render.as_mut() {
                                            render.write_mono(
                                                &decoded,
                                                microphone_rate,
                                                live.microphone_gain,
                                            )?;
                                        }
                                        if live.monitor_enabled
                                            && live.monitor_source == "headset"
                                            && plan.playback_enabled
                                        {
                                            monitor_samples.extend(resample_mono(
                                                &decoded,
                                                microphone_rate,
                                                sample_rate,
                                                live.monitor_gain,
                                            ));
                                        }
                                    }
                                    Err(error) => crate::trace::note(&format!(
                                        "microphone: skipped a damaged LC3 frame ({error})"
                                    )),
                                }
                            }
                            continue;
                        }
                    }
                    let Ok(Some(frame)) = acl.push(&raw) else { continue };

                    if crate::Link::answer_signalling(&transport, handle, 27, &frame).is_some() {
                        continue;
                    }

                    if frame.cid == crate::att::cid::ATT
                        && crate::media::serve(&transport, handle, Some(&frame.payload), 23)? { continue; }
                    // A volume button on the earcup arrives here and nowhere
                    // else, so this is the only chance to follow it.
                    if let Some(bridge) = self.volume.as_mut() {
                        if let Some((notified, value)) = notification(&frame) {
                            if notified == bridge.handles.state && bridge.absorb(value) {
                                crate::trace::note(&format!(
                                    "hlasitost ze sluchatek: {} %{}",
                                    bridge.state.percent(),
                                    if bridge.state.muted { " (ztlumeno)" } else { "" }
                                ));
                            }
                        }
                    }

                    // A read we asked for comes back without saying which
                    // handle it answers, so it is matched against the request
                    // still outstanding. Only one is ever in flight.
                    if let Some(handle) = pending_battery {
                        if frame.cid == crate::att::cid::ATT && battery_read_error(&frame.payload, handle) {
                            pending_battery = None;
                            crate::trace::note("battery read refused; retrying at the next polling interval");
                            continue;
                        }
                        if frame.cid == crate::att::cid::ATT
                            && frame.payload.first() == Some(&crate::att::att_op::READ_RESPONSE)
                        {
                            pending_battery = None;
                            let fresh = crate::link::parse_battery_level(&frame.payload[1..]);
                            let mut moved = false;
                            for (handles, level) in self.batteries.iter_mut() {
                                if handles.level == handle && fresh.is_some() && fresh != *level {
                                    *level = fresh;
                                    moved = true;
                                }
                            }
                            if moved {
                                let levels = self.battery_levels();
                                report(Progress::Battery { levels });
                            }
                            continue;
                        }
                    }

                    // Battery levels arrive the same way, and only while audio
                    // is running - this loop owns the ACL for its whole
                    // duration, so a notification not read here is a
                    // notification lost.
                    if let Some((notified, value)) = notification(&frame) {
                        // The headset announcing that it can no longer take
                        // media from us. Handled before anything else, because
                        // it is the one notification that ends this loop.
                        if self.absorb_contexts(notified, value) {
                            continue;
                        }

                        let mut moved = false;
                        for (handles, level) in self.batteries.iter_mut() {
                            if handles.level != notified {
                                continue;
                            }
                            let fresh = crate::link::parse_battery_level(value);
                            if fresh.is_some() && fresh != *level {
                                *level = fresh;
                                moved = true;
                            }
                        }
                        if moved {
                            let levels = self.battery_levels();
                            report(Progress::Battery { levels });
                        }
                    }
                }
            }

            if let Some(handle) = acl_handle { crate::media::serve(&transport, handle, None, 23)?; }
            // An underrun is not a reason to stop sending. The isochronous
            // channel has a slot every interval whether we fill it or not, and a
            // skipped slot is a hole in the audio the headphones cannot conceal.
            // Substituting silence keeps the stream in step, so the capture has
            // the next whole interval to catch up in rather than the loop
            // sliding out of phase and staying there.
            //
            // The old code slept a quarter interval and went round again, which
            // abandoned the deadline entirely: after an underrun the next real
            // frame went out immediately instead of on time, and the one after
            // that was late. That is most of the audio "cutting out" during
            // ordinary use.
            let next_samples = match wake_frame.take() {
                Some(frame) if frame.len() == encoder.samples_per_frame() * 2 => Some(frame),
                _ => capture.next_frame(encoder.samples_per_frame())?,
            };
            let mut samples = match next_samples {
                Some(samples) => samples,
                None => {
                    underruns += 1;
                    vec![0i16; encoder.samples_per_frame() * 2]
                }
            };

            if live.monitor_enabled && plan.playback_enabled {
                let wanted = samples.len() / 2;
                let monitored = if let Some(capture) = monitor_capture.as_mut() {
                    match capture.next_mono_frame(wanted, sample_rate) {
                        Ok(frame) => frame.unwrap_or_else(|| vec![0; wanted]),
                        Err(error) => {
                            crate::trace::note(&format!(
                                "monitoring skipped a frame ({error}); playback continues"
                            ));
                            vec![0; wanted]
                        }
                    }
                        .into_iter()
                        .map(|sample| {
                            (sample as f32 * live.monitor_gain.clamp(0.0, 2.0))
                                .clamp(i16::MIN as f32, i16::MAX as f32) as i16
                        })
                        .collect::<Vec<_>>()
                } else {
                    (0..wanted)
                        .map(|_| monitor_samples.pop_front().unwrap_or(0))
                        .collect::<Vec<_>>()
                };
                for (pair, microphone) in samples.chunks_exact_mut(2).zip(monitored) {
                    if live.monitor_replace {
                        pair[0] = microphone;
                        pair[1] = microphone;
                    } else {
                        pair[0] = (pair[0] as i32 + microphone as i32)
                            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        pair[1] = (pair[1] as i32 + microphone as i32)
                            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    }
                }
                // A stalled capture must not turn old sidetone into seconds of
                // delayed echo after it resumes.
                let max_monitor = sample_rate as usize / 5;
                while monitor_samples.len() > max_monitor {
                    monitor_samples.pop_front();
                }
            }

            if !plan.playback_enabled {
                clock.sleep(frame_interval / 4);
                continue;
            }

            // Screen the captured signal before an intentional boost clips it.
            // Otherwise loud but valid boosted music could be mistaken for
            // decoder garbage and stop the stream.
            let limiter = OutputLimiter::with_gain(live.output_gain.clamp(0.0, 2.0));
            if let Err(violation) = limiter.screen_interleaved(&samples, 2) {
                // Refuse the frame, not the connection. Dropping the link over
                // one 7.5 ms window means a single loud moment in a game takes
                // the headphones off someone's head, which is a far worse
                // outcome than the frame this is protecting them from. The
                // frame is silenced and the stream keeps its cadence.
                samples.iter_mut().for_each(|sample| *sample = 0);
                rejected_total += 1;
                rejected_run += 1;

                // A second of nothing but rejected frames is not a transient.
                let run_limit = (1_000_000 / plan.qos.sdu_interval_us.max(1)).max(1);
                if rejected_run >= run_limit {
                    report(Progress::Stopped { reason: violation.to_string() });
                    return Err(violation.into());
                }
                if rejected_run == 1 {
                    report(Progress::FrameRefused { total: rejected_total });
                }
            } else {
                rejected_run = 0;
            }
            limiter.apply(&mut samples);
            apply_balance(&mut samples, live.balance);
            soft_start.apply(&mut samples);

            // One packet per CIS: left to the first, right to the second. A
            // channel whose CIS never came up simply has nowhere to go, so the
            // other ear keeps playing instead of the whole stream stopping.
            let mut packets: Vec<Vec<u8>> = Vec::with_capacity(cis_handles.len());

            if dual {
                packets = encoder.stereo_packets(
                    &samples,
                    cis_handles,
                    swap_channels.load(Ordering::Relaxed),
                )?;
            } else {
                let payload = encoder.encode_interleaved(&samples)?;
                packets.push(encoder.wrap_iso_packet(cis_handles[0], &payload));
            }

            // The headset has said it can no longer accept media from us, which
            // in practice means a phone has started playing to it. That is the
            // only reason to hand it back, and it is a thing the headset says
            // rather than a thing this loop can infer: silence here is somebody
            // pausing between tracks far more often than it is somebody picking
            // up their phone, and releasing on silence spent a stream teardown
            // and rebuild every time it guessed wrong.
            //
            // Handing back has to happen out here, not in the loop: releasing
            // the endpoints means the stream this function is driving no longer
            // exists. The caller tears the group down and waits, which is also
            // the only place that can build it all again afterwards.
            if self.config.yield_when_asked && !self.media_available {
                report(Progress::Yielded);
                return Ok(());
            }

            // Track how long the stream has had nothing in it.
            if is_silent(&samples) {
                let _ = *silent_since.get_or_insert_with(Instant::now);
            } else {
                silent_since = None;
            }

            if let (Some(limit), Some(since)) = (idle_timeout, silent_since) {
                if since.elapsed() >= limit {
                    // Hand the endpoints back rather than sitting on them
                    // silently. Same exit as the yield above and for the same
                    // reason: releasing them means the stream this loop drives
                    // no longer exists, so the caller has to do it - and the
                    // caller is also the only place that can wait for sound and
                    // build the whole thing again.
                    crate::trace::note(&format!(
                        "ticho {} s - uvolnujeme endpointy, spojeni zustava",
                        limit.as_secs()
                    ));
                    report(Progress::Idle { after: limit });
                    report(Progress::Yielded);
                    return Ok(());
                }
            }

            const STALLED_AFTER: Duration = Duration::from_secs(5);
            if let Some(slot) = last_delivery.iter().position(|at| at.elapsed() >= STALLED_AFTER) {
                return Err(SessionError::IsoPath(format!(
                    "CIS {:#06x} stopped returning ISO buffers for {} seconds; rebuilding the whole stereo stream",
                    cis_handles[slot], last_delivery[slot].elapsed().as_secs())));
            }

            // Sequence numbers advance for both ears even when this interval is
            // dropped, as in AOSP. Never wait here and grow audio latency.
            let admitted = credits.reserve_frame(cis_handles);
            if !admitted {
                backpressure_frames += 1;
                if backpressure_frames == 1 || backpressure_frames % 100 == 0 {
                    crate::trace::note(&format!("ISO backpressure: {backpressure_frames} complete stereo frames dropped; controller buffers full"));
                }
            }
            let mut failure = None;
            for packet in packets.iter().filter(|_| admitted) {
                iso_sent += 1;
                if let Err(e) = transport.send_iso(packet) {
                    iso_failed += 1;
                    failure = Some(SessionError::from(e));
                    break;
                }
            }

            if let Some(e) = failure {
                // A misrouted ISO endpoint is a permanent misconfiguration, not
                // a dropped link, and reporting it as one would send us hunting
                // the radio instead of the transport. Say which it was.
                report(Progress::Stopped { reason: e.to_string() });
                return Err(e);
            }

            frames_sent += 1;

            if last_report.elapsed() >= Duration::from_secs(1) {
                let (left_db, right_db) = AudioCapture::channel_levels(&samples);
                let (bass_db, mid_db, treble_db) = AudioCapture::band_levels(&samples, sample_rate);

                report(Progress::Streaming {
                    frames: frames_sent,
                    backlog: capture.backlog(),
                    iso_sent,
                    iso_failed,
                    backpressure_frames,
                    underruns,
                    left_db,
                    right_db,
                    bass_db,
                    mid_db,
                    treble_db,
                    rssi: latest_rssi,
                    delivered: delivered.clone(),
                    quality: quality.clone(),
                });
                last_report = Instant::now();

                // A backlog that keeps growing means we are not keeping up; dropping
                // it costs a click but stops latency creeping upward forever.
                if capture.backlog() > encoder.samples_per_frame() * 8 {
                    capture.flush();
                }
            }

            // Pace to the SDU interval rather than sending as fast as we can encode.
            next_deadline += frame_interval;
            let now = Instant::now();
            if next_deadline > now {
                clock.sleep(next_deadline - now);
            } else if now.duration_since(next_deadline) > frame_interval * 4 {
                // Far enough behind that catching up frame by frame would mean a
                // burst of packets the radio cannot deliver anyway.
                next_deadline = now;
            }
            // Only slightly late: leave the deadline where it is, so the next
            // frame goes out early by the same amount and the cadence recovers
            // instead of drifting. Resetting on every small overshoot is how the
            // interval quietly becomes "however long a frame took".
        }
    }

    /// Blocks until the capture device has something audible in it.
    ///
    /// Used while the headphones belong to somebody else. Nothing is configured
    /// on them and nothing is transmitted, so this is as close to costing
    /// nothing as the stack gets: one Windows capture endpoint, examined five
    /// times a second.
    ///
    /// Returns false if the caller asked to stop first.
    pub fn wait_for_sound<S>(
        &mut self,
        acl_handle: u16,
        sample_rate: u32,
        frame_us: u32,
        mut should_stop: S,
    ) -> Result<bool>
    where
        S: FnMut() -> bool,
    {
        // How long the peer gets to finish letting go before sound is allowed
        // to pull it straight back.
        //
        // Handing the headphones back and taking them again a moment later is
        // the ordinary case, not the exception: silence between two tracks
        // outlasts the release by less than a second. But the release is not
        // instant on the device's side. Rebuilding into it produced "remote
        // device terminated connection due to low resources" - the headset
        // saying, accurately, that it had not finished freeing what we were
        // already asking for again - and a headset that has just refused a
        // channel then stops answering ATT for several seconds, so the attempt
        // after that failed too. Two attempts lost, and the third worked, which
        // is exactly what waiting here would have achieved without any of them.
        //
        // The link is serviced throughout, so this is a pause in taking the
        // headphones back, not a pause in looking after the connection.
        const SETTLE: Duration = Duration::from_secs(2);
        let started = Instant::now();

        // Whether the headset was busy when this wait began. If it was, its
        // saying so again is the event to wait for; if it was never busy, only
        // sound can end the wait.
        let was_busy = !self.media_available;
        let device = crate::audio::find_cable_device(self.config.audio_device.as_deref())?;
        let mut capture = AudioCapture::open(&device.id, sample_rate)?;

        // A frame's worth at a time, which is the same unit the encoder uses -
        // so "audible" means exactly what it means during playback and the two
        // cannot disagree about where silence ends.
        let frame = (sample_rate as usize * frame_us as usize) / 1_000_000;
        let clock = crate::audio::PreciseTimer::new();

        // The connection has to be looked after while we sit here, and this is
        // the only thing running. The peer sends a connection parameter update
        // unprompted - especially now, having just had every endpoint released,
        // because that is exactly when it wants to relax the link - and it
        // starts a timer waiting for the answer. Nothing answered it: this
        // function read the sound card and slept, and the link was gone by the
        // time anybody tried to speak on it again.
        //
        // The failure landed several steps later, on the first write of the
        // rebuilt stream, and read as "codec configuration failed" - so the
        // handing-back looked like it broke the stream setup rather than the
        // connection underneath it.
        let transport = {
            let controller = self.controller.as_ref().ok_or(SessionError::NoDeviceFound)?;
            controller.transport().clone()
        };
        let pump = {
            let controller = self.controller.as_ref().ok_or(SessionError::NoDeviceFound)?;
            controller.pump()
        };
        let mut acl = crate::att::AclReassembler::new();

        loop {
            if should_stop() {
                return Ok(false);
            }

            // A reader that has given up means no event will ever arrive again -
            // including the one that says the link has ended. Playing on would
            // be encoding into a connection nobody can confirm still exists,
            // which is precisely the failure this check exists to end: audio
            // that simply stops, with a frame counter still climbing and
            // nothing anywhere saying why.
            if !pump.alive() {
                return Err(SessionError::AdapterSilent);
            }

            // Anything the peer says, including the notice that it has given up
            // on us. Waiting for sound that will never be delivered to a link
            // that no longer exists is silence nobody can explain.
            while let Ok(event) = pump.recv_event(Duration::ZERO) {
                if let Some((closed, reason)) = crate::hci::parse_disconnection_complete(&event) {
                    if closed == acl_handle {
                        return Err(SessionError::Disconnected { reason });
                    }
                }
            }

            while let Ok(raw) = pump.recv_acl(Duration::ZERO) {
                let Ok(Some(frame)) = acl.push(&raw) else { continue };
                if crate::Link::answer_signalling(&transport, acl_handle, 27, &frame).is_some() {
                    continue;
                }

                if frame.cid == crate::att::cid::ATT
                    && crate::media::serve(&transport, acl_handle, Some(&frame.payload), 23)? { continue; }
                // Volume and battery keep arriving while we wait. Absorbing them
                // costs nothing and stops the displays freezing at whatever they
                // held when the music stopped.
                if let Some((notified, value)) = notification(&frame) {
                    // The headset offering media again: the phone has stopped.
                    if self.absorb_contexts(notified, value) {
                        continue;
                    }
                    if let Some(bridge) = self.volume.as_mut() {
                        if notified == bridge.handles.state {
                            bridge.absorb(value);
                        }
                    }
                    for (handles, level) in self.batteries.iter_mut() {
                        if handles.level == notified {
                            if let Some(fresh) = crate::link::parse_battery_level(value) {
                                *level = Some(fresh);
                            }
                        }
                    }
                }
            }

            crate::media::serve(&transport, acl_handle, None, 23)?;
            let contexts_restored = was_busy && self.media_available;

            let mut heard = None;
            while let Some(samples) = capture.next_frame(frame)? {
                if !is_silent(&samples) {
                    heard = Some(samples);
                    break;
                }
            }

            // Never take them back while the headset is still saying it cannot
            // take media. Doing so is asking a device to serve two hosts at
            // once: it refuses the channel, and refusing costs it several
            // seconds of not answering anything at all.
            let welcome = self.media_available;

            // Taking them back the moment the phone stops is the whole point:
            // the headphones stay connected the entire time, so there is
            // nothing to reconnect, only a stream to rebuild. Waiting for the
            // PC to make a sound as well would leave them idle and looking
            // disconnected until something happened to play.
            // The settling pause is only owed when the headset was busy. That
            // is the case it was measured for: a device that has just been
            // taken by a phone needs a moment before it can be asked again, and
            // rebuilding into it earns "low resources" and several seconds of
            // silence after that.
            //
            // Silence on this machine is not that case. Nobody else took the
            // headphones, nothing has to be released on their side, and waiting
            // two seconds before resuming is two seconds of missing audio every
            // time somebody presses play.
            let settled = !was_busy || started.elapsed() >= SETTLE;
            if welcome && (heard.is_some() || contexts_restored) && settled {
                if let Some(samples) = heard {
                    self.resume_capture = Some((device.id.clone(), sample_rate, capture, samples));
                }
                return Ok(true);
            }

            // Fifty milliseconds rather than two hundred. The link is being
            // serviced from this loop now, and a signalling request that waits a
            // fifth of a second for its answer is a request the peer may already
            // have given up on. It is still one wake-up per twenty, not one per
            // frame, so the saving this interval existed for is intact.
            clock.sleep(Duration::from_millis(50));
        }
    }

    /// Puts every configured ASE back to Idle before the link goes away.
    ///
    /// Without this the headphones keep their streams in Enabling or Streaming
    /// after we vanish. The next connection then tries to configure an ASE that
    /// is, from the device's point of view, already busy - an invalid state
    /// transition - and the device answers by refusing the isochronous channel.
    /// The symptom is that the first connection after starting the app works and
    /// every one after it fails, which reads as the headphones being flaky.
    ///
    /// Failures are ignored: this runs while tearing down, and a device that has
    /// already forgotten the ASE is not a problem worth reporting.
    /// Tells Source ASEs that this client is ready to receive their audio.
    ///
    /// Playback configures Sink ASEs, so that path deliberately does not call
    /// this: for a Sink ASE the server in the headphones must transition to
    /// Streaming autonomously.
    pub fn start_receivers(&mut self, link: &mut Link, control_point: u16, ase_ids: &[u8]) {
        link.set_att_timeout(Duration::from_millis(700));

        let operations: Vec<Vec<u8>> = ase_ids
            .iter()
            .map(|&ase_id| crate::bap::ascs::receiver_start_ready(ase_id))
            .collect();
        let pdu = crate::bap::ascs::batch(&operations);
        if !pdu.is_empty() && self.write_policy.check_write(control_point, &pdu).is_ok() {
            let _ = link.write_characteristic(control_point, &pdu);
        }

        link.set_att_timeout(crate::link::ATT_TIMEOUT);
    }

    /// Puts every endpoint the device has back to Idle, microphone included.
    ///
    /// A source ASE left configured by whatever connected last still holds the
    /// controller's isochronous budget, and this device only offers so much of
    /// it. Starting from a known-empty state costs two writes and removes a
    /// whole class of "it worked the first time and never again".
    ///
    /// The microphone is deliberately released rather than configured: this
    /// stack plays audio and does not record, and a source stream nobody reads
    /// is airtime taken from the two that matter.
    pub fn release_all_streams(&mut self, link: &mut Link, control_point: u16, capabilities: &AudioCapabilities) {
        let everything: Vec<u8> = capabilities
            .sink_ase_ids
            .iter()
            .chain(capabilities.source_ase_ids.iter())
            .copied()
            .collect();

        // Most clean connections already expose every endpoint as Idle. A
        // Release sent to all of them then creates work on both sides and the
        // old confirmation path could spend almost a second proving that
        // nothing needed releasing. Read the known handles once and only touch
        // endpoints that actually retain state from an earlier host/session.
        let active = match link.ase_states(&everything) {
            Ok(states) if states.len() == everything.len() => states
                .into_iter()
                .filter(|state| state.state != crate::bap::ase::STATE_IDLE)
                .map(|state| state.ase_id)
                .collect(),
            // Incomplete discovery is not proof of Idle. Preserve the defensive
            // full release in that case; reliability wins over the fast path.
            _ => everything,
        };

        self.release_streams(link, control_point, &active);
    }

    /// Hands the endpoints back and waits for the device to say it is done.
    ///
    /// The waiting is the part that matters. A write response only means the
    /// Release arrived; the device then walks the endpoint through Releasing to
    /// Idle and announces each step by notification. Dropping the link during
    /// that walk leaves the endpoint half-released as far as the headset is
    /// concerned, and a headset that still believes this host owns its audio
    /// will not route sound to a phone until it is switched off and on again.
    ///
    /// It is bounded, and failure to see Idle is not treated as an error: some
    /// firmware goes straight to Idle without a Releasing notification, and one
    /// that stays silent is not worth holding a teardown open for.
    pub fn release_streams(&mut self, link: &mut Link, control_point: u16, ase_ids: &[u8]) {
        // One atomic ASCS operation is both the specified form and substantially
        // cheaper than a separate ATT write/response for every endpoint.
        let mut asked = ase_ids.to_vec();
        asked.sort_unstable();
        asked.dedup();
        let operations: Vec<Vec<u8>> = asked
            .iter()
            .map(|&ase_id| crate::bap::ascs::release(ase_id))
            .collect();
        let pdu = crate::bap::ascs::batch(&operations);

        link.set_att_timeout(Duration::from_millis(350));
        if pdu.is_empty()
            || self.write_policy.check_write(control_point, &pdu).is_err()
            || link.write_characteristic(control_point, &pdu).is_err()
        {
            link.set_att_timeout(crate::link::ATT_TIMEOUT);
            return;
        }

        // There is no ASE subscription in the normal setup path, so waiting for
        // notifications cannot prove anything. Poll the actual state instead;
        // this is bounded because a disappearing peer must never hold shutdown.
        let deadline = Instant::now() + Duration::from_millis(900);
        while !asked.is_empty() && Instant::now() < deadline {
            if let Ok(states) = link.ase_states(&asked) {
                for state in states {
                    if matches!(state.state, crate::bap::ase::STATE_IDLE | crate::bap::ase::STATE_CODEC_CONFIGURED) {
                        asked.retain(|&id| id != state.ase_id);
                    }
                }
            }
            if !asked.is_empty() {
                std::thread::sleep(Duration::from_millis(75));
            }
        }

        if !asked.is_empty() {
            crate::trace::note(&format!(
                "Release was sent, but ASE(s) {:?} did not report Idle before the teardown deadline",
                asked
            ));
        }

        link.set_att_timeout(crate::link::ATT_TIMEOUT);
    }

    /// Ends a connection properly, so the peer can be reached again.
    ///
    /// Waits briefly for the controller to confirm. Returning before the
    /// Disconnection Complete arrives means the next connection attempt races
    /// the teardown, which is how a reconnect ends up failing for reasons that
    /// have nothing to do with the peer.
    pub fn disconnect(&mut self, handle: u16) {
        crate::media::reset_link();
        self.resume_capture = None;
        let Some(controller) = self.controller.as_mut() else {
            return;
        };

        let command = crate::hci::disconnect(handle, crate::hci::REASON_REMOTE_USER_TERMINATED);
        if controller.command(&command).is_err() {
            return;
        }

        // Short: this runs while tearing down, and a peer that has already gone
        // will never answer. Waiting the full command timeout here is most of
        // why disconnecting felt like the program had frozen.
        let _ = controller.wait_for_event(Duration::from_millis(800), |event| {
            crate::hci::parse_disconnection_complete(event)
                .map(|(closed, _)| closed == handle)
                .unwrap_or(false)
        });
    }

    /// Releases the adapter, so something else can open it.
    ///
    /// Dropping the session is not enough on its own: the pump's reader threads
    /// are blocked inside a read and each holds the transport alive. Waking them
    /// first is what actually closes the handle.
    pub fn shutdown(&mut self) {
        crate::media::reset_link();
        self.resume_capture = None;
        if let Some(controller) = self.controller.as_ref() {
            controller.pump().stop();
        }
        self.controller = None;
        self.last_cis_handles.clear();
        self.volume = None;
        self.batteries.clear();
        self.contexts_handle = None;
        self.available_contexts = None;
        self.active_context = crate::bap::ascs::CONTEXT_MEDIA;
        self.media_available = true;
        self.write_policy = WritePolicy::default();
    }

    /// The configuration this session is running with.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Lets a caller adjust settings between steps.
    ///
    /// Deliberately not a free-for-all setter on every field: the app changes
    /// scan length and little else while a session is alive, and everything
    /// baked into a stream is fixed when the stream is planned.
    pub fn config_mut(&mut self) -> &mut SessionConfig {
        &mut self.config
    }

    pub fn controller_mut(&mut self) -> Option<&mut Controller> {
        self.controller.as_mut()
    }

    /// Builds a link for GATT work over an established connection.
    pub fn open_link(&self, handle: u16) -> Result<Link> {
        let controller = self.controller.as_ref().ok_or(SessionError::NoDeviceFound)?;
        let transport = controller.transport().clone();
        // Deliberately the controller's own pump. Starting a second one here put
        // two threads on the same endpoints, and each ACL packet went to
        // whichever won the race - so pairing and GATT reads timed out waiting
        // for replies the command loop had already thrown away.
        Ok(Link::new(transport, controller.pump(), handle))
    }
}

/// Human-readable summary of what a device published.
pub fn describe_capabilities(capabilities: &AudioCapabilities) -> String {
    let records: Vec<_> = capabilities
        .sink_records
        .iter()
        .filter(|r| r.is_lc3())
        .collect();

    if records.is_empty() {
        return "zadne LC3 sink capabilities".into();
    }

    // A device publishes one record per sample rate, so summarising only the
    // first one hides everything it can really do - and reads as if the rates
    // further down the list were missing entirely.
    let mut rates: Vec<u32> = records
        .iter()
        .flat_map(|r| r.capabilities.sampling_frequencies.iter())
        .filter_map(|f| f.hz())
        .collect();
    rates.sort_unstable();
    rates.dedup();

    let rate_list = rates
        .iter()
        .map(|hz| format!("{} kHz", hz / 1000))
        .collect::<Vec<_>>()
        .join(", ");

    // Best case across every record: the widest frame it will take.
    let max_octets = records
        .iter()
        .filter_map(|r| r.capabilities.max_octets_per_frame)
        .max()
        .unwrap_or(0);

    let stereo = if records
        .iter()
        .any(|r| r.capabilities.supports_stereo_in_one_stream())
    {
        "stereo in one stream: yes"
    } else {
        "stereo na jednom streamu: ne"
    };

    format!(
        "{} zaznamu; frekvence: {rate_list}; nejvyse {max_octets} B/ramec; {stereo};          sink ASE: {:?}; mikrofon (source ASE): {:?}; usi: {}",
        records.len(),
        capabilities.sink_ase_ids,
        capabilities.source_ase_ids,
        describe_locations(capabilities.sink_locations)
    )
}

/// Which ears the device says it has.
///
/// Worth printing rather than parsing once and forgetting. A device that only
/// claims one side cannot render the other however correctly the streams are
/// configured, and that failure is indistinguishable from a routing bug in the
/// host: one ear plays everything and the other stays silent.
pub fn describe_locations(locations: Option<u32>) -> String {
    let Some(bits) = locations else {
        return "neuvedeno".into();
    };

    let mut sides = Vec::new();
    if bits & crate::bap::LOCATION_FRONT_LEFT != 0 {
        sides.push("left");
    }
    if bits & crate::bap::LOCATION_FRONT_RIGHT != 0 {
        sides.push("right");
    }

    if sides.is_empty() {
        return format!("none ({bits:#010x})");
    }

    format!("{} ({bits:#010x})", sides.join(" + "))
}

/// Picks the device that looks most like the headphones the user means.
pub fn best_match<'a>(
    devices: &'a [DiscoveredDevice],
    name_hint: Option<&str>,
) -> Option<&'a DiscoveredDevice> {
    if let Some(hint) = name_hint {
        let lowered = hint.to_lowercase();
        if let Some(found) = devices.iter().find(|d| {
            d.name
                .as_ref()
                .map(|n| n.to_lowercase().contains(&lowered))
                .unwrap_or(false)
        }) {
            return Some(found);
        }
    }

    // Otherwise prefer something that announced LE Audio, and within that the
    // strongest signal - almost always the nearest device.
    devices
        .iter()
        .max_by_key(|d| (d.is_le_audio(), d.rssi))
}

/// Parses an address in the usual display form.
pub fn parse_address(text: &str) -> Option<BdAddr> {
    BdAddr::parse(text)
}

/// Keep both results while draining the event batch: an ACL loss takes
/// precedence even when its dependent CIS disconnects arrive afterwards.
#[derive(Default)]
struct AudioDisconnects {
    acl: Option<u8>,
    cis: Option<(u16, u8)>,
}

impl AudioDisconnects {
    fn observe(&mut self, acl: Option<u16>, channels: &[u16], handle: u16, reason: u8) {
        if acl == Some(handle) {
            self.acl = Some(reason);
        } else if channels.contains(&handle) {
            self.cis = Some((handle, reason));
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::synthetic_capabilities;

    #[test]
    fn audio_disconnect_does_not_imply_acl_loss() {
        let mut loss = AudioDisconnects::default();
        loss.observe(Some(1), &[2, 3], 2, 0x08);
        assert_eq!(loss.acl, None);
        assert_eq!(loss.cis, Some((2, 0x08)));
        loss.observe(Some(1), &[2, 3], 9, 0x13);
        assert_eq!(loss.acl, None);
        assert_eq!(loss.cis, Some((2, 0x08)));
    }

    #[test]
    fn acl_disconnect_keeps_its_reason_in_either_event_order() {
        for events in [[(1, 0x08), (2, 0x16)], [(2, 0x16), (1, 0x08)]] {
            let mut loss = AudioDisconnects::default();
            for (handle, reason) in events {
                loss.observe(Some(1), &[2, 3], handle, reason);
            }
            assert_eq!(loss.acl, Some(0x08));
        }
    }

    #[test]
    fn locations_are_reported_in_words() {
        use crate::bap::{LOCATION_FRONT_LEFT, LOCATION_FRONT_RIGHT, LOCATION_STEREO};

        assert_eq!(describe_locations(Some(LOCATION_STEREO)), "left + right (0x00000003)");
        assert_eq!(describe_locations(Some(LOCATION_FRONT_LEFT)), "left (0x00000001)");
        assert_eq!(describe_locations(Some(LOCATION_FRONT_RIGHT)), "right (0x00000002)");

        // A device claiming no ears at all is a real answer, not a missing one.
        assert_eq!(describe_locations(Some(0)), "none (0x00000000)");
        assert_eq!(describe_locations(None), "neuvedeno");
    }

    #[test]
    fn capability_summary_mentions_what_matters() {
        let caps = synthetic_capabilities(true, 2);
        let summary = describe_capabilities(&caps);

        assert!(summary.contains("48 kHz"));
        assert!(summary.contains("stereo in one stream: yes"));
        assert!(summary.contains("sink ASE: [1, 2]"), "{summary}");
        // The microphone is listed too: an endpoint nobody mentions is an
        // endpoint nobody thinks to release.
        assert!(summary.contains("mikrofon"), "{summary}");
    }

    #[test]
    fn device_without_lc3_is_described_plainly() {
        let caps = AudioCapabilities::default();
        assert_eq!(describe_capabilities(&caps), "zadne LC3 sink capabilities");
    }

    #[test]
    fn name_hint_wins_over_signal_strength() {
        let devices = vec![
            DiscoveredDevice {
                address: BdAddr([1, 0, 0, 0, 0, 0]),
                address_type: 0,
                rssi: -30, // much closer
                name: Some("Nekde jinde".into()),
                appearance: None,
                service_uuids: vec![0x1850],
            },
            DiscoveredDevice {
                address: BdAddr([2, 0, 0, 0, 0, 0]),
                address_type: 0,
                rssi: -80,
                name: Some("JBL Tune 780NC".into()),
                appearance: None,
                service_uuids: vec![0x1850],
            },
        ];

        let picked = best_match(&devices, Some("JBL")).unwrap();
        assert_eq!(picked.name.as_deref(), Some("JBL Tune 780NC"));

        // With no hint, the nearest device wins.
        let nearest = best_match(&devices, None).unwrap();
        assert_eq!(nearest.rssi, -30);
    }

    #[test]
    fn balance_only_ever_turns_one_side_down() {
        let mut frame = vec![10_000i16, 10_000, -10_000, -10_000];
        apply_balance(&mut frame, 0.5);
        assert_eq!(frame, vec![5_000, 10_000, -5_000, -10_000], "right untouched");

        let mut frame = vec![10_000i16, 10_000];
        apply_balance(&mut frame, -1.0);
        assert_eq!(frame, vec![10_000, 0], "fully left silences the right");

        let mut frame = vec![10_000i16, -10_000];
        apply_balance(&mut frame, 0.0);
        assert_eq!(frame, vec![10_000, -10_000], "centre changes nothing");
    }

    #[test]
    fn dither_still_counts_as_silence() {
        // What Windows actually sends when nothing is playing.
        let quiet: Vec<i16> = (0..480).map(|i| if i % 7 == 0 { 1 } else { 0 }).collect();
        assert!(is_silent(&quiet));

        // Quiet music is not silence, however quiet.
        let mut faint = vec![0i16; 480];
        faint[100] = 900;
        assert!(!is_silent(&faint));
    }

    #[test]
    fn the_reconnect_window_eventually_gives_up() {
        let policy = ReconnectPolicy::default();

        assert!(policy.enabled);
        assert!(policy.should_retry(Duration::from_secs(0)));
        assert!(policy.should_retry(Duration::from_secs(899)));
        assert!(!policy.should_retry(Duration::from_secs(900)));
        // Derived rather than written out: the attempt count is the window
        // divided by the interval, and pinning the answer meant that shortening
        // the interval - which is a change to how quickly it retries, not to
        // how long it keeps trying - failed here for no real reason.
        let expected = policy.window.map(|w| (w.as_secs() / policy.interval.as_secs()) as u32);
        assert_eq!(policy.attempts_in_window(), expected);
    }

    #[test]
    fn reconnect_can_be_turned_off_and_made_endless() {
        assert!(!ReconnectPolicy::disabled().should_retry(Duration::ZERO));
        assert!(ReconnectPolicy::forever().should_retry(Duration::from_secs(86_400)));
    }

    #[test]
    fn a_disconnection_we_asked_for_is_not_retried() {
        // Local host terminated, and the peer powering off, are both decisions.
        assert!(!ReconnectPolicy::worth_reconnecting(0x16));
        assert!(!ReconnectPolicy::worth_reconnecting(0x15));

        // Out of range and a peer-side drop are both worth chasing.
        assert!(ReconnectPolicy::worth_reconnecting(0x08));
        assert!(ReconnectPolicy::worth_reconnecting(0x13));
    }

    #[test]
    fn silence_releases_the_endpoints_soon_enough_to_be_useful() {
        // Five minutes was the old value, and it meant the endpoints were held
        // through every realistic pause - so in practice they were never
        // released and a phone could never take the headphones. The number has
        // to be short enough that reaching for a phone works, and long enough
        // that a gap between two tracks does not cost a teardown.
        let timeout = SessionConfig::default().idle_timeout.expect("silence must release");
        assert!(timeout >= Duration::from_secs(5), "too eager: {timeout:?}");
        assert!(timeout <= Duration::from_secs(30), "too slow to be usable: {timeout:?}");
    }

    #[test]
    fn the_headphones_are_handed_back_when_they_ask_and_not_before() {
        // On by default: a headset this host never lets go of is a headset a
        // phone cannot use, and that is the common case for anyone who owns
        // both.
        assert!(SessionConfig::default().yield_when_asked);
    }

    #[test]
    fn a_device_that_publishes_nothing_is_never_treated_as_busy() {
        // Silence about availability is not a refusal. Reading it as one would
        // mean never playing to any headset that does not publish contexts.
        let session = Session::new(SessionConfig::default());
        assert!(session.media_available(), "unknown must mean free");
    }

    #[test]
    fn the_default_plays_at_full_quality() {
        let config = SessionConfig::default();

        // Attenuating by default cost three bits of resolution before the
        // encoder ever saw the signal. The soft-start ramp and the garbage
        // screen are what protect the listener now.
        assert_eq!(config.limiter.gain(), 1.0, "the default must be transparent");

        // Two streams, because that is the layout Windows negotiates and the
        // only one this stack has been proven against. The plan builder still
        // falls back to one stream for a device with a single Sink ASE.
        assert!(!config.prefer_single_cis, "the proven topology is two streams");
    }

    #[test]
    fn address_parses_from_display_form() {
        let address = parse_address("7C:FE:62:72:B4:9A").unwrap();
        assert_eq!(address.to_string(), "7C:FE:62:72:B4:9A");
        assert!(parse_address("nonsense").is_none());
    }
}

#[cfg(test)]
mod audit_regression_tests {
    use super::*;
    #[test]
    fn silence_must_not_release_an_active_microphone() {
        let timeout = Some(Duration::from_secs(10));
        assert_eq!(silence_release_timeout(timeout, true), None);
        assert_eq!(silence_release_timeout(timeout, false), timeout);
        assert_eq!(silence_release_timeout(None, false), None);
    }

    #[test]
    fn only_a_matching_battery_read_error_completes_the_request() {
        assert!(battery_read_error(&[0x01, 0x0a, 0x34, 0x12, 0x05], 0x1234));
        assert!(!battery_read_error(&[0x01, 0x0a, 0x34, 0x12, 0x05], 0x5678));
        assert!(!battery_read_error(&[0x01, 0x12, 0x34, 0x12, 0x05], 0x1234));
        assert!(!battery_read_error(&[0x01, 0x0a], 0x1234));
    }

}

#[cfg(test)]
mod transport_improvement_tests {
    use super::*;
    #[test]
    fn battery_fallback_rotates_and_skips_notification_services() {
        let batteries = vec![
            (crate::link::BatteryHandles { level: 1, level_cccd: 2, notifies: true }, Some(90)),
            (crate::link::BatteryHandles { level: 3, level_cccd: 0, notifies: false }, Some(80)),
            (crate::link::BatteryHandles { level: 4, level_cccd: 0, notifies: false }, Some(70)),
        ];
        assert_eq!(next_battery_index(&batteries, 0, false), Some(1));
        assert_eq!(next_battery_index(&batteries, 2, false), Some(2));
        assert_eq!(next_battery_index(&batteries, 0, true), Some(0));
        assert_eq!(next_battery_index(&[], 0, true), None);
        assert_eq!(next_battery_index(&batteries[..1], 0, false), None);
    }

}
