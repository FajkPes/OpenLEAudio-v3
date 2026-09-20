//! The process the configuration app talks to.
//!
//! The app is WinUI 3 and cannot drive WinUSB isochronous pipes; this stack is
//! Rust and has no business drawing a window. So they are two processes with a
//! pipe between them, speaking one JSON object per line in each direction.
//!
//! Why a pipe rather than a DLL the app calls into: the session is deliberately
//! single threaded - the HCI pump, the link and the encoder all share one
//! `Rc`-based world and cannot be touched from a UI thread. A pipe makes that
//! constraint structural instead of something the app has to remember. It also
//! means a crash in the radio stack closes a pipe rather than taking the window
//! with it.
//!
//! Commands arrive on stdin, events leave on stdout, and anything the stack
//! wants to say to a human goes to stderr so it never corrupts the protocol.

use std::io::{BufRead, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use olea_core::bonding::{Bond, BondStore};
use olea_core::bap::Preset;
use olea_core::safety::OutputLimiter;
use olea_core::controller::DiscoveredDevice;
use olea_core::session::{
    describe_capabilities, LiveAudioConfig, MetricsLevel, Progress, ReconnectPolicy, Session,
    SessionConfig,
};
use olea_core::settings::Settings;
use olea_core::stream::StreamPlan;
use serde_json::{json, Value};

fn main() {
    let media_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let media_worker_flag = media_running.clone();
    olea_core::media::enable(Settings::load(&settings_path()).bool("media_controls_enabled").unwrap_or(true));
    let media_worker = std::thread::spawn(move || {
        while media_worker_flag.load(std::sync::atomic::Ordering::Relaxed) {
            for command in olea_core::media::take_commands() {
                emit(json!({"event":"media-command", "id":command.id,"opcode":command.opcode,"position":command.position}));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    let (commands_tx, commands_rx) = channel::<Value>();

    // Playback blocks the worker for as long as it runs, so anything that has to
    // interrupt it cannot go through the same queue - it would be read only once
    // the thing it is meant to stop has already finished. This flag is shared
    // with the worker and cleared here, on the thread that is always listening.
    let playing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Cuts a reconnect wait short. Raised for any command that means the user
    // has moved on, so nobody has to sit out a three-minute retry window.
    let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Flipped from this thread while audio is running, so the effect is heard
    // the moment the switch moves. Going through the command queue would mean
    // waiting for playback to end - which is exactly the thing being judged.
    let swap_channels = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let live_audio = std::sync::Arc::new(std::sync::RwLock::new(LiveAudioConfig::default()));

    // Set from this thread for the same reason: while audio runs, the worker is
    // inside the playback loop and would not read a queued command until the
    // music stopped - which is not when anyone wants to know their battery
    // level.
    let battery_refresh = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Counts the commands that mean "stop what you were asked to do". A connect
    // that was queued before one of them is stale by the time the worker reaches
    // it, and running it anyway is why pressing Disconnect could be followed by
    // the headphones connecting again on their own.
    let cancel_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Which device the worker currently holds, readable from this thread.
    //
    // Needed for exactly one decision: Unpair on the device that is playing has
    // to stop it, and Unpair on any other device must not. Without knowing
    // which, the choice is between a button that does nothing until the music
    // ends and one that cuts off unrelated playback.
    let connected_address =
        std::sync::Arc::new(std::sync::RwLock::new(Option::<String>::None));

    // The session owns `Rc`s and must never leave the thread that built it.
    let worker = {
        let playing = playing.clone();
        let interrupt = interrupt.clone();
        let swap_channels = swap_channels.clone();
        let live_audio = live_audio.clone();
        let battery_refresh = battery_refresh.clone();
        let cancel_epoch = cancel_epoch.clone();
        let connected_address = connected_address.clone();
        std::thread::spawn(move || {
            run_worker(
                commands_rx,
                playing,
                interrupt,
                swap_channels,
                battery_refresh,
                cancel_epoch,
                live_audio,
                connected_address,
            )
        })
    };

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(line) {
            Ok(mut command) => {
                let name = command
                    .get("cmd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                // Anything that has to interrupt playback is noticed here, on
                // the thread that is always listening. Disconnect belongs in
                // this list: while audio runs the worker is blocked, so a
                // queued disconnect would only be read once the thing it is
                // meant to end had ended on its own.
                // Playback deliberately occupies the radio worker. Settings
                // must not sit behind that hours-long operation: persist and
                // acknowledge them on this always-listening thread, then leave
                // a lightweight reload command for the worker. This is also
                // what makes a named LC3 preset immediately update every value
                // shown in the UI while the current stream keeps playing.
                if name == "validate-settings" || name == "connect"
                    || (name == "disconnect" && command.get("validate").and_then(Value::as_bool)==Some(true)) {
                    let address=command.get("address").and_then(Value::as_str).unwrap_or("");
                    let saved=Settings::load(&settings_path());
                    let valid=report_preflight(address,&saved);
                    if name=="validate-settings" || !valid {
                        emit(json!({"event":"done","cmd":name}));continue;
                    }
                }
                if name == "media-status" {
                    olea_core::media::update(olea_core::media::Snapshot {
                        player:command.get("player").and_then(Value::as_str).unwrap_or("Windows").to_string(),
                        title:command.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
                        state:command.get("state").and_then(Value::as_u64).unwrap_or(0).min(3) as u8,
                        duration:command.get("duration").and_then(Value::as_i64).unwrap_or(-1).clamp(-1,i32::MAX as i64) as i32,
                        position:command.get("position").and_then(Value::as_i64).unwrap_or(0).clamp(0,i32::MAX as i64) as i32,
                        supported:command.get("supported").and_then(Value::as_u64).unwrap_or(0) as u32,
                    });
                    continue;
                }
                if name == "media-result" {
                    olea_core::media::finish(command.get("id").and_then(Value::as_u64).unwrap_or(0),
                        command.get("success").and_then(Value::as_bool).unwrap_or(false));
                    continue;
                }
                if name == "set" {
                    let key = command.get("key").and_then(Value::as_str).unwrap_or("");
                    let value = command.get("value").and_then(Value::as_str).unwrap_or("");
                    let mut saved = Settings::load(&settings_path());
                    match apply_setting_change(&mut saved, key, value)
                        .and_then(|needs| {
                            saved.save(&settings_path())
                                .map_err(|e| format!("save failed: {e}"))?;
                            Ok(needs)
                        })
                    {
                        Ok(needs) => {
                            sync_live_from_settings(&live_audio, &saved);
                            if let Ok(address)=connected_address.read() {
                                if let Some(address)=address.as_deref() {report_preflight(address,&saved);}
                            }
                            swap_channels.store(
                                saved.bool("swap_channels").unwrap_or(false),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        emit(json!({
                            "event": "applied",
                            "key": key,
                            "value": value,
                                "needs": needs,
                        }));
                            command["prePersisted"] = json!(true);
                        }
                        Err(text) => {
                            emit(json!({ "event": "error", "cmd": "set", "text": text }));
                            emit(json!({ "event": "done", "cmd": "set" }));
                            continue;
                        }
                    }
                }

                if name == "settings" {
                    let saved = Settings::load(&settings_path());
                    emit_settings_snapshot(&saved);
                    emit(json!({ "event": "done", "cmd": "settings" }));
                    continue;
                }

                if name == "battery" {
                    battery_refresh.store(true, std::sync::atomic::Ordering::Relaxed);
                }

                if name == "debug" {
                    if command.get("on").and_then(Value::as_bool).unwrap_or(false) {
                        olea_core::trace::enable();
                    } else {
                        olea_core::trace::disable();
                    }
                }

                let forgetting_the_connected_device = name == "forget"
                    && command
                        .get("address")
                        .and_then(Value::as_str)
                        .zip(connected_address.read().ok())
                        .map(|(address, held)| held.as_deref() == Some(address))
                        .unwrap_or(false);

                if matches!(name.as_str(), "stop" | "quit" | "disconnect")
                    || forgetting_the_connected_device
                    || (name == "adapter"
                        && command.get("on").and_then(Value::as_bool) == Some(false))
                {
                    playing.store(false, std::sync::atomic::Ordering::Relaxed);
                }

                // Also anything that means the user has chosen something else to
                // do. A queued command cannot be read while the worker sits in a
                // reconnect wait, so the wait has to be told from here.
                if matches!(
                    name.as_str(),
                    "stop" | "quit" | "disconnect" | "connect" | "forget" | "scan" | "adapter"
                ) {
                    interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
                }

                // Every command carries the epoch it was accepted in. The
                // worker compares it against the current one and skips anything
                // the user has since changed their mind about.
                if matches!(name.as_str(), "disconnect" | "stop" | "quit" | "forget" | "adapter") {
                    cancel_epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                command["epoch"] =
                    json!(cancel_epoch.load(std::sync::atomic::Ordering::Relaxed));

                if commands_tx.send(command).is_err() {
                    break;
                }
                // `quit` is also an instruction to this stdin owner. Waiting
                // for another line here kept the process alive after the radio
                // worker had already exited; an application restart could then
                // leave an orphan holding redirected pipes (and, on less tidy
                // shutdown paths, the adapter). Dropping the sender below lets
                // the worker consume the queued quit and joins it normally.
                if name == "quit" {
                    break;
                }
            }
            Err(e) => emit(json!({ "event": "error", "text": format!("unreadable command: {e}") })),
        }
    }

    // Standard input closing means the app is gone. Stopping playback first is
    // not tidiness: the worker spends playback blocked inside `run_audio`, so
    // joining without this waits forever, and the agent keeps running as an
    // orphan - still holding the adapter. The next launch then finds the device
    // taken and reports it as "still owned by another driver", which sends the
    // investigation to the driver binding instead of to this process.
    playing.store(false, std::sync::atomic::Ordering::Relaxed);
    interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(commands_tx);
    media_running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = media_worker.join();
    let _ = worker.join();
}

/// Writes one event. Flushed immediately: the app is waiting on this line.
fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn log(text: impl Into<String>) {
    emit(json!({ "event": "log", "text": text.into() }));
}

fn useful_device_name(name: &str, address: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case(address)
        && !trimmed.eq_ignore_ascii_case("(unnamed)")
        && !trimmed.eq_ignore_ascii_case("(bez jmena)")
}

/// Only an explicit authentication/key error permits replacement pairing.
/// A timeout, USB failure or disconnect leaves the existing bond intact.
fn saved_key_rejected(error: &olea_core::session::SessionError) -> bool {
    matches!(error, olea_core::session::SessionError::Controller(
        olea_core::controller::ControllerError::EncryptionRejected { status: 0x05 | 0x06 }
    ))
}

/// Errors caused solely by saved choices cannot improve by reconnecting. A
/// previous implementation retried these for the whole reconnect window, so a
/// custom value outside the PAC range became an endless connect/fail loop that
/// also kept waking the headphones. Transient radio and teardown failures stay
/// retryable; deterministic capability mismatches stop and wait for a change.
fn deterministic_configuration_error(error: &str) -> bool {
    [
        "device rejected the custom configuration:",
        "selected gaming mode is not supported",
        "selected media mode is not supported",
        "stream could not be scheduled:",
        "headset microphone could not be configured:",
        "custom transport latency exceeds headset limit",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
}

fn sync_live_from_settings(
    live_audio: &std::sync::Arc<std::sync::RwLock<LiveAudioConfig>>,
    settings: &Settings,
) {
    let Ok(mut live) = live_audio.write() else { return };
    live.monitor_enabled = settings.bool("monitor_enabled").unwrap_or(false);
    live.monitor_source = settings.get("monitor_source").unwrap_or("default").to_owned();
    live.monitor_replace = settings.get("monitor_mode").unwrap_or("mix") == "replace";
    live.monitor_gain = settings.number("monitor_gain").unwrap_or(1.0);
    live.output_gain = settings.number("gain").unwrap_or(1.0);
    live.microphone_gain = settings.number("microphone_gain").unwrap_or(1.0);
    live.balance = (settings.number("balance").unwrap_or(0.0) / 50.0).clamp(-1.0, 1.0);
    let minutes = settings.number("battery_poll_min").unwrap_or(15.0);
    live.battery_poll_override = Some((minutes >= 1.0)
        .then(|| Duration::from_secs_f32(minutes.clamp(1.0, 120.0) * 60.0)));
    live.metrics_override = Some(settings.get("link_metrics")
        .and_then(MetricsLevel::from_setting).unwrap_or_default());
}

/// Everything the worker remembers between commands.
struct Agent {
    session: Option<Session>,
    bonds: BondStore,
    settings: Settings,
    found: Vec<DiscoveredDevice>,
    /// Cleared from the reading thread to interrupt playback.
    playing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Raised by the reading thread when a command arrives that must cut a
    /// reconnect wait short. Separate from `playing` because the two answer
    /// different questions, and the old code used one flag for both: a
    /// reconnect wait therefore claimed audio was running, and a stray
    /// disconnect during setup silenced a stream nobody had started.
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the reading thread so it takes effect during playback.
    swap_channels: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Raised when the user clicks the battery indicator.
    ///
    /// The audio loop owns the link while music plays, so a battery read cannot
    /// be issued from here. It is left as a request the loop picks up on its
    /// next pass, which is at most one frame away.
    battery_refresh: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Raised by the reading thread for anything that supersedes a connection.
    ///
    /// The retry loop can be running for a quarter of an hour, and for most of
    /// that it is inside a radio window rather than in a wait that the interrupt
    /// flag can cut short. Comparing this at the top of every round is what
    /// makes Unpair actually stop the reconnect it was meant to stop, instead of
    /// the headphones pairing themselves again a few seconds later.
    cancel_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    live_audio: std::sync::Arc<std::sync::RwLock<LiveAudioConfig>>,
    /// Kept alive after connecting: the GATT link is what configures the stream,
    /// and dropping it would tear down everything the audio needs.
    link: Option<olea_core::Link>,
    connected: Option<(String, u16)>,
    /// The address half of `connected`, mirrored where the reading thread can
    /// see it. Written only through `hold` and `release_hold`, so the two
    /// cannot drift apart.
    connected_mirror: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// The ASEs this connection configured, so they can be released again.
    configured_ases: Vec<u8>,
    /// Read once per connection. Rediscovering the same services before every
    /// stream costs a second of round trips and floods the event queue for no
    /// new information.
    capabilities: Option<olea_core::AudioCapabilities>,
    control_point: Option<u16>,
    /// Set only when the controller reports that the peer link ended itself.
    /// A user stop deliberately leaves this empty and must never trigger retry.
    audio_stable: bool,
    lost_reason: Option<u8>,
    /// Set when playback ended because the headphones were handed back to
    /// whatever else wants them, rather than because anything went wrong.
    yielded: Option<(u32, u32)>,
    /// Whether the last attempt got as far as an open ACL connection.
    ///
    /// "The headphones are not in range" and "the headphones are here but
    /// refused the stream" both arrive as an error string, and retrying them at
    /// the same rate is wrong in both directions: the first wants the radio
    /// listening almost continuously, the second wants to stop hammering a
    /// device that is busy tidying up.
    reached_link: bool,
}

fn run_worker(
    commands: Receiver<Value>,
    playing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    swap_channels: std::sync::Arc<std::sync::atomic::AtomicBool>,
    battery_refresh: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cancel_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    live_audio: std::sync::Arc<std::sync::RwLock<LiveAudioConfig>>,
    connected_mirror: std::sync::Arc<std::sync::RwLock<Option<String>>>,
) {
    let mut agent = Agent {
        session: None,
        bonds: BondStore::load(&BondStore::default_path()),
        settings: Settings::load(&settings_path()),
        found: Vec::new(),
        playing,
        interrupt,
        swap_channels,
        battery_refresh,
        cancel_epoch: cancel_epoch.clone(),
        live_audio,
        link: None,
        connected: None,
        connected_mirror,
        configured_ases: Vec::new(),
        capabilities: None,
        control_point: None,
        audio_stable: false,
        lost_reason: None,
        yielded: None,
        reached_link: false,
    };

    agent.swap_channels.store(
        agent.settings.bool("swap_channels").unwrap_or(false),
        std::sync::atomic::Ordering::Relaxed,
    );
    agent.sync_live_audio();

    emit(json!({ "event": "ready", "paired": agent.bonds.len() }));

    // Before the app asks for anything. Every one of these problems produces a
    // confusing failure later rather than an obvious one now, so the honest
    // moment to raise them is the first.
    let _ = agent.check_environment();

    while let Ok(command) = commands.recv() {
        let name = command.get("cmd").and_then(Value::as_str).unwrap_or("");

        // A connect that was asked for before the user pressed Disconnect is no
        // longer what they want. Acknowledged rather than silently dropped, so
        // the app clears its spinner instead of waiting for an answer that is
        // never coming.
        let stamped = command.get("epoch").and_then(Value::as_u64).unwrap_or(0);
        if name == "connect"
            && stamped < cancel_epoch.load(std::sync::atomic::Ordering::Relaxed)
        {
            log("connect request dropped: something else was asked for in the meantime");
            emit(json!({ "event": "done", "cmd": name }));
            continue;
        }

        let result = match name {
            "status" => agent.status(),
            "check" => agent.check_environment(),
            "adapter" => agent.adapter(command.get("on").and_then(Value::as_bool).unwrap_or(true)),
            "scan" => agent.scan(command.get("seconds").and_then(Value::as_u64).unwrap_or(8)),
            "connect" => agent.connect(&command),
            "forget" => agent.forget(&command),
            "settings" => agent.report_settings(),
            "set" => agent.set(&command),
            "reset-settings" => agent.reset_settings(),
            "play" => agent.play(),
            "disconnect" => agent.disconnect(),
            "debug" => Ok(()),
            // Handled on the reading thread, which raised the flag the audio
            // loop reads. Nothing left to do here.
            "battery" => Ok(()),
            "stop" => Ok(()),
            "quit" => break,
            other => Err(format!("unknown command '{other}'")),
        };

        if let Err(text) = result {
            // Include the command so the UI can clear the matching spinner and
            // connecting state. A text-only error left a failed device row in
            // "Connecting..." forever even though the worker had already stopped.
            emit(json!({ "event": "error", "cmd": name, "text": text }));
        }
        emit(json!({ "event": "done", "cmd": name }));
    }
}

/// The reconnect policy as it stands on disk right now.
///
/// Read rather than remembered because the worker thread is occupied for the
/// whole life of a connection: settings are persisted by the reading thread,
/// and this is the only way a change to them can reach a loop that is already
/// running.
fn reconnect_policy_from_disk() -> ReconnectPolicy {
    let settings = Settings::load(&settings_path());
    let defaults = ReconnectPolicy::default();

    ReconnectPolicy {
        enabled: settings.bool("reconnect_enabled").unwrap_or(defaults.enabled),
        interval: settings
            .number("reconnect_interval_s")
            .map(|seconds| seconds.clamp(1.0, 60.0))
            .map(Duration::from_secs_f32)
            .unwrap_or(defaults.interval),
        window: settings
            .minutes("reconnect_window_min")
            .unwrap_or(defaults.window),
    }
}

fn audio_recovery_policy(settings: &Settings) -> ReconnectPolicy {
    ReconnectPolicy {
        enabled: settings.bool("audio_recovery_enabled").unwrap_or(true),
        interval: Duration::from_secs_f32(settings.number("audio_recovery_interval_s").unwrap_or(2.0).clamp(1.0, 60.0)),
        window: settings.minutes("audio_recovery_window_min").unwrap_or(None),
    }
}

#[derive(Clone)]
struct CachedQos {
    codec: olea_core::bap::CodecConfiguration, phy:u8, target_latency:u8, context:u16,
    min_delay:u32, max_delay:u32, max_transport:Option<u16>,
}
#[derive(Clone)]
struct KnownDevice { caps:olea_core::AudioCapabilities, qos:Vec<CachedQos> }
fn known_devices() -> &'static std::sync::Mutex<std::collections::HashMap<String,KnownDevice>> {
    static CACHE:std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String,KnownDevice>>>=std::sync::OnceLock::new();
    CACHE.get_or_init(||std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn preflight_for(address:&str,settings:&Settings)->Result<String,String> {
    let cache=known_devices().lock().map_err(|_|"capability cache unavailable")?;
    let Some(device)=cache.get(&address.to_ascii_uppercase()) else {
        return Ok("Device capabilities are not known yet; they must be checked after connection.".into());
    };
    let (plan,_)=olea_core::preflight::plan(settings,&device.caps,false)?;
    if settings.get("preset")==Some("custom") {
        if let Some(q)=device.qos.iter().find(|q|q.codec==plan.codec && q.phy==plan.qos.phy && q.target_latency==plan.target_latency && q.context==plan.context) {
            if !(q.min_delay..=q.max_delay).contains(&plan.qos.presentation_delay_us) {
                return Err(format!("Presentation delay: selected {} ms; headset permits {}–{} ms for this codec and PHY",plan.qos.presentation_delay_us as f64/1000.0,q.min_delay as f64/1000.0,q.max_delay as f64/1000.0));
            }
            if q.max_transport.is_some_and(|max|plan.qos.max_transport_latency_ms>max) {
                return Err(format!("Transport latency exceeds the headset limit of {} ms",q.max_transport.unwrap()));
            }
            return Ok("Codec, channel layout, safety limits and known QoS bounds passed. Controller scheduling and headset acceptance still require negotiation.".into());
        }
    }
    Ok("Codec, channel layout and safety limits passed. QoS for this combination still requires negotiation.".into())
}
fn report_preflight(address:&str,settings:&Settings)->bool {
    match preflight_for(address,settings) {
        Ok(text)=>{emit(json!({"event":"preflight","address":address,"valid":true,"text":text}));true},
        Err(text)=>{emit(json!({"event":"preflight","address":address,"valid":false,"text":text}));false},
    }
}

fn connection_authorized(favorite: bool, confirmed_once: &mut bool) -> bool {
    let confirmed = std::mem::take(confirmed_once);
    favorite || confirmed
}
fn is_favorite(address: &str) -> bool {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return false; };
    let path = std::path::PathBuf::from(local).join("OpenLEAudio").join("favorites.json");
    std::fs::read_to_string(path).ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .is_some_and(|items| items.iter().any(|item| item.eq_ignore_ascii_case(address)))
}

fn settings_path() -> std::path::PathBuf {
    BondStore::default_path().with_file_name("settings.txt")
}

fn apply_setting_change(settings: &mut Settings, key: &str, value: &str) -> Result<Vec<Value>, String> {
    let before = settings.clone();
    if olea_core::settings::knob(key).is_none() {
        return Err(format!("unknown setting '{key}'"));
    }
    if value.len() > 512 || value.contains('\r') || value.contains('\n') {
        return Err("setting value is too long or contains a line break".into());
    }
    if key == "preset"
        && !matches!(value, "google" | "custom" | "windows" | "high-quality" | "low-latency" | "robust")
    {
        return Err("unknown stream configuration".into());
    }
    if key == "audio_context" && !matches!(value, "media" | "game") {
        return Err("unknown audio context".into());
    }
    if key == "audio_mode" && !matches!(value, "stereo" | "mono") {
        return Err("unknown audio mode".into());
    }
    if key == "microphone_mode" && !matches!(value, "off" | "on") {
        return Err("unknown microphone mode".into());
    }
    if key == "microphone_quality" && !matches!(value, "voice" | "balanced" | "high") {
        return Err("unknown microphone quality".into());
    }
    if key == "microphone_target"
        && !matches!(value, "vb-cable" | "vb-cable-a" | "vb-cable-b" | "none")
    {
        return Err("unknown microphone target".into());
    }
    if key == "monitor_mode" && !matches!(value, "mix" | "replace") {
        return Err("unknown monitoring mode".into());
    }
    if key == "link_metrics"
        && MetricsLevel::from_setting(value).is_none()
    {
        return Err("unknown radio monitoring level".into());
    }
    if key == "command_style"
        && olea_core::transport::CommandStyle::from_setting(value).is_none()
    {
        return Err("unknown HCI command addressing mode".into());
    }
    if key == "acl_phy" && !matches!(value, "auto" | "1M" | "2M") {
        return Err("ACL PHY must be auto, 1M or 2M".into());
    }
    if key == "phy" && !matches!(value, "1M" | "2M") {
        return Err("radio PHY must be 1M or 2M".into());
    }
    if key == "frame_ms" && !matches!(value, "7.5" | "10" | "10.0") {
        return Err("LC3 frame duration must be 7.5 or 10 ms".into());
    }
    if key == "rate_hz"
        && !matches!(value, "8000" | "16000" | "24000" | "32000" | "44100" | "48000")
    {
        return Err("unsupported LC3 sample rate".into());
    }
    if matches!(
        key,
        "audio_recovery_enabled" | "reconnect_enabled" | "startup_reconnect_enabled" | "diagnostics"
            | "swap_channels" | "multipoint_yield_enabled" | "monitor_enabled"
            | "run_in_background" | "start_with_windows" | "media_controls_enabled"
    ) && !matches!(value, "true" | "false" | "1" | "0" | "yes" | "ne")
    {
        return Err("boolean setting must be true or false".into());
    }

    let numeric_range = match key {
        "octets" => Some((20.0, 400.0)),
        "retransmissions" => Some((0.0, 15.0)),
        "max_latency_ms" => Some((5.0, 4_000.0)),
        "presentation_delay_ms" => Some((0.0, 4_000.0)),
        "gain" | "microphone_gain" | "monitor_gain" => Some((0.0, 2.0)),
        "balance" => Some((-50.0, 50.0)),
        "idle_link_latency" => Some((0.0, 30.0)),
        "battery_poll_min" => Some((0.0, 120.0)),
        "link_timeout_s" => Some((2.0, 30.0)),
        "idle_timeout_min" => Some((0.0, 120.0)),
        "reconnect_interval_s" | "audio_recovery_interval_s" => Some((1.0, 60.0)),
        "reconnect_window_min" | "audio_recovery_window_min" => Some((0.0, 1_440.0)),
        _ => None,
    };
    if let Some((min, max)) = numeric_range {
        let number = value.parse::<f32>().map_err(|_| format!("{key} must be a number"))?;
        if !number.is_finite() || !(min..=max).contains(&number) {
            return Err(format!("{key} must be between {min} and {max}"));
        }
    }

    settings.set(key, value);
    if key == "media_controls_enabled" { olea_core::media::enable(settings.bool(key).unwrap_or(true)); }
    if key == "microphone_mode"
        && value == "off"
        && settings.get("monitor_source") == Some("headset")
    {
        settings.set("monitor_source", "default");
    }

    let automatic_changed = (key == "preset" && value != "custom")
        || (key == "audio_context" && settings.get("preset") != Some("custom"));
    if automatic_changed {
        // The codec knobs mirror whatever the automatic choice would ask for,
        // so the Sound panel shows the stream that is actually going to be
        // configured rather than whatever was left there by an earlier edit.
        //
        // For the Android path that is the head of its Media list. The real
        // choice still happens per device at connect time - a headset that
        // refuses 48_4 gets the next entry that fits - but the head of the list
        // is what a capable device gets and what belongs on screen as the
        // default. Leaving these values stale was what made switching to
        // Google/Android look like it did nothing at all.
        let (codec, qos) = if settings.get("preset") == Some("google") {
            let order = if settings.get("audio_context") == Some("game") {
                olea_core::google::GAME_ORDER
            } else {
                olea_core::google::MEDIA_ORDER
            };
            let head = order
                .first()
                .expect("Android scenario order is never empty");

            let codec = olea_core::bap::CodecConfiguration {
                sampling_frequency: head.setting.sampling_frequency,
                frame_duration: head.setting.frame_duration,
                channel_allocation: olea_core::bap::LOCATION_FRONT_LEFT,
                octets_per_frame: head.setting.octets_per_frame,
                frames_per_sdu: 1,
            };

            // Android writes zero for the retransmission count and the
            // transport latency and fills them in from what the headset
            // publishes when it answers. Nothing is connected here, so what is
            // shown is the fallback used when a device states no preference.
            let qos = head.qos.resolve(&codec, None);
            (codec, qos)
        } else {
            // Legacy values saved by version 1 - "windows", "robust" and the
            // rest. Those presets are gone; rather than failing and leaving the
            // panel showing a configuration nothing will send, they land on the
            // Android path like everything else that is not custom.
            let head = olea_core::google::MEDIA_ORDER
                .first()
                .expect("the Media order is never empty");

            let codec = olea_core::bap::CodecConfiguration {
                sampling_frequency: head.setting.sampling_frequency,
                frame_duration: head.setting.frame_duration,
                channel_allocation: olea_core::bap::LOCATION_FRONT_LEFT,
                octets_per_frame: head.setting.octets_per_frame,
                frames_per_sdu: 1,
            };
            let qos = head.qos.resolve(&codec, None);
            settings.set("preset", "google");
            (codec, qos)
        };

        settings.set("rate_hz", codec.sampling_frequency.hz().unwrap_or(48_000).to_string());
        settings.set(
            "frame_ms",
            if codec.frame_duration.microseconds() == 7_500 { "7.5" } else { "10" },
        );
        settings.set("octets", codec.octets_per_frame.to_string());
        settings.set("phy", if qos.phy == 0x01 { "1M" } else { "2M" });
        settings.set("retransmissions", qos.retransmission_number.to_string());
        settings.set("max_latency_ms", qos.max_transport_latency_ms.to_string());
        settings.set("presentation_delay_ms", (qos.presentation_delay_us / 1000).to_string());
    }

    Ok(settings
        .scopes_touched_by(&before)
        .iter()
        .map(|knob| json!({ "key": knob.key, "scope": knob.scope.explain() }))
        .collect())
}

fn emit_settings_snapshot(settings: &Settings) {
    let capture_devices = olea_core::audio::list_capture_devices().unwrap_or_default();
    let playback_options: Vec<Value> = capture_devices
        .iter()
        .map(|device| json!({ "value": device.id, "label": device.name }))
        .collect();
    let mut monitor_options = vec![json!({
        "value": "default",
        "label": "Windows default microphone"
    })];
    monitor_options.extend(
        capture_devices
            .iter()
            .filter(|device| !device.is_virtual_cable())
            .map(|device| json!({ "value": device.id, "label": device.name })),
    );
    if settings.get("microphone_mode").unwrap_or("off") == "on" {
        monitor_options.push(json!({
            "value": "headset",
            "label": "Headset microphone (LE Audio)"
        }));
    }
    let knobs: Vec<Value> = olea_core::settings::KNOBS
        .iter()
        .map(|knob| {
            let mut item = json!({
                "key": knob.key,
                "description": knob.description,
                "scope": knob.scope.explain(),
                "value": settings.get(knob.key).unwrap_or(""),
            });
            if knob.key == "playback_source" {
                item["options"] = json!(playback_options);
            } else if knob.key == "monitor_source" {
                item["options"] = json!(monitor_options);
            }
            item
        })
        .collect();
    emit(json!({ "event": "settings", "knobs": knobs }));
}

impl Agent {
    fn status(&mut self) -> Result<(), String> {
        emit(json!({
            "event": "status",
            "adapterOn": self.session.is_some(),
            "paired": self.bonds.all().map(|b| json!({
                "address": b.address,
                "name": b.name,
                "leAudio": b.le_audio,
            })).collect::<Vec<_>>(),
        }));
        Ok(())
    }

    /// Reports everything standing between here and working audio.
    fn check_environment(&mut self) -> Result<(), String> {
        let issues = olea_core::environment::check(self.settings.get("playback_source"));

        // Reported as an event only. The check runs on startup and again
        // whenever the radio is asked for, so logging from here wrote the same
        // complaints out three times before anyone had done anything; the app
        // writes them once, when they change.
        emit(json!({
            "event": "environment",
            "issues": issues
                .iter()
                .map(|issue| json!({
                    "id": issue.id,
                    "severity": issue.severity.as_str(),
                    "summary": issue.summary,
                    "remedy": issue.remedy,
                    "setupAction": issue.setup_action,
                }))
                .collect::<Vec<_>>(),
        }));

        Ok(())
    }

    /// Turns the radio on or off, which for this stack means owning the adapter.
    ///
    /// Off drops the session entirely rather than sending a reset: the adapter
    /// then belongs to nobody, which is the state Windows can take it back from.
    fn adapter(&mut self, on: bool) -> Result<(), String> {
        if !on {
            // Everything holding the adapter has to go, in order: the audio
            // loop, then the GATT link, then the reader threads, then the
            // session. Leaving any of them behind keeps the device open and the
            // next attempt to switch Bluetooth back on is refused.
            let _ = self.disconnect();
            if let Some(session) = self.session.as_mut() {
                session.shutdown();
            }
            self.session = None;
            self.found.clear();
            emit(json!({ "event": "adapter", "on": false }));
            return Ok(());
        }

        if self.session.is_some() {
            return self.status();
        }

        self.settings = Settings::load(&settings_path());
        self.sync_live_audio();

        // Checked here rather than left to the transport. Opening an adapter
        // that is still on the Microsoft Bluetooth stack fails with "no
        // controller found", which reads as a missing or broken adapter and
        // sends people looking at their hardware. Say what is actually true and
        // which Setup step fixes it, and refuse rather than half start.
        let blocking: Vec<_> = olea_core::environment::check(self.settings.get("playback_source"))
            .into_iter()
            .filter(|issue| issue.severity == olea_core::environment::Severity::Blocking)
            .collect();

        if let Some(issue) = blocking.first() {
            let _ = self.check_environment();
            emit(json!({ "event": "adapter", "on": false }));
            return Err(format!("{} {}", issue.summary, issue.remedy));
        }

        let mut session = Session::new(self.session_config());
        let mut detail = json!({ "event": "adapter", "on": true });

        session
            .open_adapter(|progress| {
                if let Progress::AdapterReady { version, address } = progress {
                    detail["version"] = json!(version);
                    detail["address"] = json!(address);
                }
            })
            .map_err(|e| format!("adapter could not be enabled: {e}"))?;

        self.session = Some(session);
        emit(detail);
        Ok(())
    }

    fn session_config(&self) -> SessionConfig {
        let defaults = SessionConfig::default();
        let command_style = self.settings.get("command_style")
            .and_then(olea_core::transport::CommandStyle::from_setting)
            .unwrap_or(defaults.command_style);

        SessionConfig {
            command_style,
            prefer_single_cis: false,
            audio_device: self.settings.get("playback_source").map(str::to_owned),
            limiter: self
                .settings
                .number("gain")
                .map(OutputLimiter::with_gain)
                .unwrap_or_default(),
            microphone_target: match self.settings.get("microphone_target") {
                Some("vb-cable") => Some("CABLE Input".to_string()),
                Some("vb-cable-a") => Some("CABLE-A Input".to_string()),
                Some("vb-cable-b") => Some("CABLE-B Input".to_string()),
                _ => None,
            },
            microphone_gain: self.settings.number("microphone_gain").unwrap_or(1.0),
            monitor_source: self
                .settings
                .bool("monitor_enabled")
                .unwrap_or(false)
                .then(|| self.settings.get("monitor_source").unwrap_or("default").to_owned()),
            monitor_replace: self.settings.get("monitor_mode").unwrap_or("mix") == "replace",
            monitor_gain: self.settings.number("monitor_gain").unwrap_or(1.0),
            live_audio: self.live_audio.clone(),
            idle_timeout: self
                .settings
                .minutes("idle_timeout_min")
                .unwrap_or(defaults.idle_timeout),
            metrics: self.metrics_level(),
            yield_when_asked: self.yield_when_asked(),
            link_timeout: self.link_timeout(),
            battery_refresh: self.battery_refresh.clone(),
            battery_poll: self.battery_poll(),
            idle_link_latency: self.idle_link_latency(),
            swap_channels: self.swap_channels.clone(),
            reconnect: ReconnectPolicy {
                enabled: self.settings.bool("reconnect_enabled").unwrap_or(true),
                interval: self
                    .settings
                    .number("reconnect_interval_s")
                    .map(|seconds| seconds.clamp(1.0, 60.0))
                    .map(Duration::from_secs_f32)
                    .unwrap_or(defaults.reconnect.interval),
                window: self
                    .settings
                    .minutes("reconnect_window_min")
                    .unwrap_or(defaults.reconnect.window),
            },
            ..defaults
        }
    }

    /// Scans, reporting each device as it is seen and whether we already know it.
    fn scan(&mut self, seconds: u64) -> Result<(), String> {
        let bonds = &self.bonds;
        let interrupt = self.interrupt.clone();
        // This scan is the newest command, so consume the flag raised when it
        // was accepted. A later Connect/Disconnect raises it again.
        interrupt.store(false, std::sync::atomic::Ordering::Relaxed);
        let session = self
            .session
            .as_mut()
            .ok_or("adapter is off")?;

        session.config_mut().scan_duration = Duration::from_secs(seconds.clamp(1, 60));

        let devices = session
            .scan_interruptible(|progress| {
                if let Progress::DeviceFound { name, address, address_type, rssi, le_audio } = progress {
                    let matched = olea_core::BdAddr::parse(&address).and_then(|a|
                        bonds.all().find(|bond| bond.matches(a, address_type)));
                    let address = matched.map(|bond| bond.address.clone()).unwrap_or(address);
                    emit(json!({
                        "event": "device",
                        "address": address,
                        "name": name,
                        "rssi": rssi,
                        "leAudio": le_audio,
                        "paired": bonds.contains(&address),
                    }));
                }
            }, || interrupt.load(std::sync::atomic::Ordering::Relaxed))
            .map_err(|e| format!("scan failed: {e}"))?;

        self.found = devices;
        Ok(())
    }

    /// Connects to a device, pairing first only when we do not already know it,
    /// and keeps it connected for as long as the reconnect policy allows.
    ///
    /// One loop covers three things that used to be handled separately and
    /// inconsistently: the first attempt, an attempt that fell over during
    /// stream setup, and a link that dropped after playing happily for an hour.
    /// The old code only retried the last of those - a setup failure returned an
    /// error straight to the app and automatic reconnect never ran at all, which
    /// is why reconnecting by hand worked and letting the stack do it did not.
    fn connect(&mut self, command: &Value) -> Result<(), String> {
        let address = command
            .get("address")
            .and_then(Value::as_str)
            .ok_or("address is missing")?
            .to_string();

        let mut confirmed_once = command.get("confirmed").and_then(Value::as_bool).unwrap_or(false);

        self.refresh_connection_config()?;
        // Proves the adapter is on before anything else runs; the policy itself
        // is read from disk at the top of every round.
        self.session.as_ref().ok_or("adapter is off")?;
        // Assigned at the top of every round, from disk. Declared here only so
        // it outlives one iteration.
        let mut policy: ReconnectPolicy;

        // Cleared here rather than by the reader thread: this is the command the
        // interrupt was raised for, and leaving it set would cancel the very
        // connection it asked for.
        self.interrupt.store(false, std::sync::atomic::Ordering::Relaxed);

        // The window is measured from the moment contact was lost, and it starts
        // again every time the headphones come back. Someone who wears them all
        // day should not find the stack has quietly stopped trying because of a
        // drop that happened this morning.
        let mut retrying_since = Instant::now();
        let mut attempt = 0u32;

        // What "current" means for this connection. Anything that raises it -
        // Disconnect, Unpair, turning the adapter off - ends this loop at the
        // next opportunity rather than at the end of the retry window.
        let started_in = self.cancel_epoch.load(std::sync::atomic::Ordering::Relaxed);

        // How long the controller is left listening for the peer to advertise.
        //
        // The first attempt gets a generous window: someone has just pressed
        // Connect and the headphones are in their hand. Every attempt after
        // that uses the retry interval instead, so the radio is listening for
        // almost the whole of it rather than sleeping through most of it and
        // then asking once. A connection then completes the moment the
        // headphones walk back into range, not up to an interval later.
        let first_window = Duration::from_secs(15);

        loop {
            if self.cancel_epoch.load(std::sync::atomic::Ordering::Relaxed) > started_in {
                log("connection attempt stopped: something else was asked for");
                emit(json!({ "event": "reconnect-stopped", "address": address }));
                return Ok(());
            }

            // Unpairing has to end this too. The bond is what a reconnect uses,
            // and without this check the loop simply pairs again from scratch -
            // which is exactly what the user had just undone.
            if attempt > 0 && !self.bonds.contains(&address) {
                log("automatic reconnect stopped: the device is no longer paired");
                emit(json!({ "event": "reconnect-stopped", "address": address }));
                return Ok(());
            }

            // Re-read every round. The worker is inside this loop for as long as
            // the headphones are connected, so a policy captured once would mean
            // "reconnect off" only taking effect after the next disconnection -
            // which is the one place it can no longer be used.
            policy = reconnect_policy_from_disk();
            if attempt >= 2 && !policy.enabled {
                emit(json!({ "event": "reconnect-stopped", "address": address }));
                return Ok(());
            }

            // Everything else the next connection depends on, re-read for the
            // same reason: link timeout, codec, radio, multipoint, metrics. Read
            // once before the loop, a setting changed while the stack was
            // retrying only took effect after a manual Disconnect and Connect -
            // and retrying is exactly when someone reaches for the settings.
            if attempt > 0 {
                self.refresh_connection_config()?;
            }

            if !connection_authorized(is_favorite(&address), &mut confirmed_once) {
                emit(json!({"event":"connection-confirmation-required", "address":address}));
                return Ok(());
            }
            attempt += 1;
            let already_paired = self.bonds.contains(&address);
            if attempt > 1 {
                emit(json!({ "event": "reconnecting", "address": address, "attempt": attempt }));
            }
            let window = if attempt == 1 {
                first_window
            } else {
                policy
                    .interval
                    .clamp(Duration::from_millis(1_500), Duration::from_secs(20))
            };

            let device = self.target_device(&address)?;
            match self.connect_device(&address, &device, already_paired, window) {
                Ok(()) => {
                    // Playback ended. Either the user asked for that, or the
                    // link went away underneath it.
                    match self.lost_reason.take() {
                        Some(reason) if ReconnectPolicy::worth_reconnecting(reason) => {
                            // Playback may have lasted hours; use the user's current policy.
                            policy = reconnect_policy_from_disk();
                            log(format!(
                                "connection lost: {}",
                                olea_core::hci::disconnect_reason(reason)
                            ));
                            // Contact existed until a moment ago, so the clock
                            // for giving up starts now.
                            retrying_since = Instant::now();
                            attempt = 0;

                            // Say so immediately. The next attempt gets the full
                            // fifteen-second listening window and, being the
                            // first of its round, used to announce nothing at
                            // all - so the app showed a plainly disconnected
                            // device for fifteen seconds while the radio was
                            // already listening for it. Whether it succeeds or
                            // not, someone watching deserves to know it is
                            // being tried.
                            // Only when something is actually going to happen.
                            // Announcing a reconnect and then returning because
                            // the feature is off leaves the app waiting for an
                            // end that never comes.
                            if policy.enabled {
                                emit(json!({
                                    "event": "reconnect-started",
                                    "address": address,
                                    "intervalMs": policy.interval.as_millis() as u64,
                                }));
                            }
                        }
                        Some(reason) => {
                            log(format!(
                                "connection ended: {} - not reconnecting",
                                olea_core::hci::disconnect_reason(reason)
                            ));
                            return Ok(());
                        }
                        // A clean stop asked for by the user.
                        None => return Ok(()),
                    }
                }
                Err(error) => {
                    self.lost_reason = None;
                    // Whatever went wrong, the peer may be left holding half a
                    // connection. Hand everything back before asking again, so
                    // a retry is as clean as pressing Disconnect and Connect.
                    self.reset_after_failure();

                    if deterministic_configuration_error(&error) {
                        log("saved stream settings are incompatible with this device; automatic retries are paused until the settings change");
                        return Err(error);
                    }

                    // The very first attempt gets one immediate retry and then
                    // reports. Two clean attempts cost about a second between
                    // them and cover the common case of the peer still tidying
                    // up the previous session; anything past that is a real
                    // failure the user needs to see rather than watch a spinner
                    // for.
                    if attempt == 1 {
                        log(format!("attempt failed: {error}; trying once more"));
                        if !self.wait_for(Duration::from_millis(600)) {
                            return Ok(());
                        }
                        continue;
                    }

                    if attempt == 2 && !policy.enabled {
                        return Err(error);
                    }

                    if attempt == 2 {
                        // Automatic reconnect takes over from here. Say so once,
                        // so the app can show "reconnecting" instead of an error
                        // that is about to be retried anyway.
                        log(format!("attempt failed: {error}"));
                        emit(json!({
                            "event": "reconnect-started",
                            "address": address,
                            "intervalMs": policy.interval.as_millis() as u64,
                        }));
                        retrying_since = Instant::now();
                    } else {
                        log(format!("attempt failed: {error}"));
                    }
                }
            }

            if !policy.enabled {
                return Ok(());
            }

            if !policy.should_retry(retrying_since.elapsed()) {
                log("automatic reconnect ended: retry window expired");
                emit(json!({ "event": "reconnect-stopped", "address": address }));
                return Ok(());
            }

            // A peer that never answered has already cost a full listening
            // window, so the next attempt starts almost at once - the interval
            // has effectively already elapsed with the radio doing something
            // useful. A peer that answered and then refused the stream is a
            // different matter: it is busy, and asking again immediately is how
            // one bad attempt becomes a run of them.
            let pause = if self.reached_link {
                policy.interval
            } else {
                Duration::from_millis(400)
            };

            if pause >= Duration::from_secs(1) {
                log(format!("next attempt in {:.1} s", pause.as_secs_f32()));
            }

            if !self.wait_for(pause) {
                log("automatic reconnect canceled");
                emit(json!({ "event": "reconnect-stopped", "address": address }));
                return Ok(());
            }
        }
    }

    /// The device to aim at, from this session's scan or from what we already know.
    ///
    /// A reconnect must not depend on a scan having happened. The bond stores
    /// the address, and a bonded peer can be connected to directly - requiring a
    /// fresh scan first is both slower and a reason for automatic reconnect to
    /// fail with "start a scan first" at the exact moment nobody is watching.
    fn target_device(&mut self, address: &str) -> Result<DiscoveredDevice, String> {
        if let Some(bond) = self.bonds.get(address).filter(|b| b.identity.is_some()).cloned() {
            let interrupt = self.interrupt.clone();
            if let Some(device) = self.session.as_mut().ok_or("adapter is off")?
                .find_bond(&bond, || interrupt.load(std::sync::atomic::Ordering::Relaxed))
                .map_err(|e| format!("private address search failed: {e}"))? {
                return Ok(device);
            }
            let identity = bond.identity.as_ref().unwrap();
            return Ok(DiscoveredDevice { address: identity.address, address_type: identity.address_type,
                name: Some(bond.name), rssi: 0, appearance: None, service_uuids: Vec::new() });
        }
        if let Some(device) = self
            .found
            .iter()
            .find(|d| d.address.to_string().eq_ignore_ascii_case(address))
        {
            return Ok(device.clone());
        }

        let bond = self
            .bonds
            .get(address)
            .ok_or("device is not in the scan results; start a scan first")?;

        let parsed = olea_core::hci::BdAddr::parse(address)
            .ok_or("address could not be read")?;

        Ok(DiscoveredDevice {
            address: parsed,
            address_type: bond.address_type,
            name: Some(bond.name.clone()),
            rssi: 0,
            appearance: None,
            service_uuids: Vec::new(),
        })
    }

    /// Waits, letting the reader thread cut it short. False means "give up".
    ///
    /// The old code borrowed the playback flag for this. That flag means
    /// "audio is running", and overloading it meant a reconnect wait looked like
    /// playback to everything else that consults it - including the shutdown
    /// path, which then waited for audio that was never going to start.
    fn wait_for(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            if self.interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.interrupt.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Puts our side back to nothing after a failed attempt.
    ///
    /// Everything here is best effort by design: the peer may already be gone,
    /// and an error while tidying up must not replace the error worth reporting.
    /// What matters is that the next attempt starts from the same state a fresh
    /// launch would - which is precisely why connecting by hand was more
    /// reliable than the automatic path that skipped this.
    fn reset_after_failure(&mut self) {
        let handle = self.release_hold().map(|(_, handle)| handle);

        if let (Some(session), Some(link), Some(control_point)) = (
            self.session.as_mut(),
            self.link.as_mut(),
            self.control_point,
        ) {
            let ases = std::mem::take(&mut self.configured_ases);
            if !ases.is_empty() {
                session.release_streams(link, control_point, &ases);
            }
        }

        self.configured_ases.clear();
        self.link = None;
        self.capabilities = None;
        self.control_point = None;

        if let (Some(session), Some(handle)) = (self.session.as_mut(), handle) {
            session.disconnect(handle);
        }
    }

    /// Applies settings whose documented scope is the next connection.
    fn refresh_connection_config(&mut self) -> Result<(), String> {
        self.settings = Settings::load(&settings_path());
        self.sync_live_audio();
        let defaults = SessionConfig::default();
        // Read before the session is borrowed mutably: both want `self`.
        let metrics = self.metrics_level();
        let yield_when_asked = self.yield_when_asked();
        let link_timeout = self.link_timeout();
        let battery_poll = self.battery_poll();
        let idle_link_latency = self.idle_link_latency();
        let session = self.session.as_mut().ok_or("adapter is off")?;
        let config = session.config_mut();

        // `device` is the preferred Bluetooth headset. It must never select the
        // Windows audio source (that old key collision is what made HyperX
        // QuadCast replace the music capture).
        config.audio_device = self.settings.get("playback_source").map(str::to_owned);
        config.microphone_target = match self.settings.get("microphone_target") {
            Some("vb-cable") => Some("CABLE Input".to_string()),
            Some("vb-cable-a") => Some("CABLE-A Input".to_string()),
            Some("vb-cable-b") => Some("CABLE-B Input".to_string()),
            _ => None,
        };
        config.microphone_gain = self.settings.number("microphone_gain").unwrap_or(1.0);
        config.monitor_source = self
            .settings
            .bool("monitor_enabled")
            .unwrap_or(false)
            .then(|| self.settings.get("monitor_source").unwrap_or("default").to_owned());
        config.monitor_replace = self.settings.get("monitor_mode").unwrap_or("mix") == "replace";
        config.monitor_gain = self.settings.number("monitor_gain").unwrap_or(1.0);
        config.live_audio = self.live_audio.clone();
        config.limiter = OutputLimiter::with_gain(
            self.settings.number("gain").unwrap_or(defaults.limiter.gain()),
        );
        config.idle_timeout = self
            .settings
            .minutes("idle_timeout_min")
            .unwrap_or(defaults.idle_timeout);
        config.metrics = metrics;
        config.yield_when_asked = yield_when_asked;
        config.link_timeout = link_timeout;
        config.battery_refresh = self.battery_refresh.clone();
        config.battery_poll = battery_poll;
        config.idle_link_latency = idle_link_latency;
        config.reconnect = ReconnectPolicy {
            enabled: self.settings.bool("reconnect_enabled").unwrap_or(true),
            interval: Duration::from_secs_f32(
                self.settings
                    .number("reconnect_interval_s")
                    .unwrap_or(defaults.reconnect.interval.as_secs_f32())
                    .clamp(1.0, 60.0),
            ),
            window: self
                .settings
                .minutes("reconnect_window_min")
                .unwrap_or(defaults.reconnect.window),
        };

        Ok(())
    }

    /// How long the link may go unheard before it counts as lost.
    ///
    /// Clamped to what the specification allows rather than trusted: the file is
    /// editable by hand, and a controller answers an impossible value with
    /// "invalid HCI parameters" from a command that names nothing.
    fn link_timeout(&self) -> Duration {
        let seconds = self.settings.number("link_timeout_s").unwrap_or(10.0);
        Duration::from_secs_f32(seconds.clamp(2.0, 30.0))
    }

    /// How often to ask for the battery level, or `None` for "never ask".
    fn battery_poll(&self) -> Option<Duration> {
        let minutes = self.settings.number("battery_poll_min").unwrap_or(15.0);
        (minutes >= 1.0).then(|| Duration::from_secs_f32(minutes.clamp(1.0, 120.0) * 60.0))
    }

    /// How many control-channel wake-ups the headphones may skip while playing.
    fn idle_link_latency(&self) -> u16 {
        self.settings
            .number("idle_link_latency")
            .unwrap_or(0.0)
            .clamp(0.0, 30.0) as u16
    }

    fn metrics_level(&self) -> MetricsLevel {
        self.settings
            .get("link_metrics")
            .and_then(MetricsLevel::from_setting)
            .unwrap_or_default()
    }

    /// How long silence lasts before the headphones are handed back, if ever.
    fn yield_when_asked(&self) -> bool {
        self.settings.bool("multipoint_yield_enabled").unwrap_or(false)
    }

    fn sync_live_audio(&self) {
        sync_live_from_settings(&self.live_audio, &self.settings);
    }

    /// Establishes, configures and plays one physical connection attempt.
    fn connect_device(
        &mut self,
        address: &str,
        device: &DiscoveredDevice,
        already_paired: bool,
        radio_window: Duration,
    ) -> Result<(), String> {
        self.lost_reason = None;
        self.reached_link = false;
        // Advertising names are optional and may disappear between scans. Do
        // not replace a useful paired name with the address just because this
        // particular advertisement was nameless.
        let advertised_name = device.display_name();
        let stored_bond = self.bonds.get(address).cloned();
        let friendly_name = if useful_device_name(&advertised_name, address) {
            advertised_name
        } else {
            stored_bond
                .as_ref()
                .map(|bond| bond.name.clone())
                .filter(|name| useful_device_name(name, address))
                .unwrap_or_else(|| address.to_string())
        };
        let le_audio = device.is_le_audio()
            || stored_bond.as_ref().map(|bond| bond.le_audio).unwrap_or(false);
        // Cloned before the session is borrowed, so the closure below owns what
        // it reads rather than borrowing `self` a second time.
        //
        // Interruptible, because this is the long one. The listening window is
        // up to fifteen seconds and until now nothing could shorten it: pressing
        // Disconnect set the flag and then waited for the radio to finish
        // listening anyway, which looked exactly like the program having hung.
        let interrupt = self.interrupt.clone();
        let cancel_epoch = self.cancel_epoch.clone();
        let epoch_now = cancel_epoch.load(std::sync::atomic::Ordering::Relaxed);

        let session = self.session.as_mut().ok_or("adapter is off")?;

        log(if already_paired {
            format!("connecting {friendly_name} (already paired)")
        } else {
            format!("pairing {friendly_name}")
        });

        let handle = session
            .connect_within_until(&device, radio_window, |p| report(p), || {
                interrupt.load(std::sync::atomic::Ordering::Relaxed)
                    || cancel_epoch.load(std::sync::atomic::Ordering::Relaxed) > epoch_now
            })
            .map_err(|e| format!("connection failed: {e}"))?;

        // Past this point a failure is about configuring a peer we can talk to,
        // not about a peer that is not there. The two deserve very different
        // retry behaviour and this is the only place that can tell them apart.
        self.reached_link = true;

        let mut link = match session.open_link(handle) {
            Ok(link) => link,
            Err(e) => {
                session.disconnect(handle);
                return Err(format!("connection could not be opened: {e}"));
            }
        };

        let stored_key = self
            .bonds
            .get(address)
            .map(|bond| bond.long_term_key)
            .filter(|key| key.iter().any(|&byte| byte != 0));

        let mut reused_bond = false;
        let key_result = if let Some(key) = stored_key {
            match session.resume_encryption(handle, &key) {
                Ok(()) => {
                    reused_bond = true;
                    log("restored an encrypted connection from the saved bond");
                    Ok(key)
                }
                Err(error) if saved_key_rejected(&error) => {
                    log(format!("saved key rejected ({error}); attempting fresh pairing"));
                    session.pair(&mut link, handle, device)
                        .map_err(|e| format!("pairing again failed: {e}"))
                }
                Err(error) => Err(format!("encryption could not be restored: {error}")),
            }
        } else {
            session.pair(&mut link, handle, device)
                .map_err(|e| format!("pairing failed: {e}"))
        };

        let long_term_key = match key_result {
            Ok(key) => key,
            Err(error) => {
                session.disconnect(handle);
                return Err(error);
            }
        };

        // Remember it only once the key actually exists, so a failed attempt
        // never leaves a bond that cannot be used.
        self.bonds.insert(Bond {
            identity: if reused_bond { stored_bond.as_ref().and_then(|b| b.identity.clone()) }
                else { session.peer_identity.take() },
            address: address.to_string(),
            // From the advertisement that got us here. Without it a later
            // reconnect has to guess, and guessing wrong never completes.
            address_type: stored_bond.as_ref().map(|b| b.address_type).unwrap_or(device.address_type),
            name: friendly_name.clone(),
            long_term_key,
            le_audio,
        });
        let _ = self.bonds.save(&BondStore::default_path());
        // The bond exists now, before the slower PACS/ASCS discovery starts.
        // Let the UI move the row immediately instead of showing a successfully
        // paired headset under "discovered" for several more seconds.
        emit(json!({
            "event": "paired",
            "address": address,
            "name": friendly_name,
            "leAudio": le_audio,
        }));

        let session = self.session.as_mut().ok_or("adapter is off")?;
        let acl_phy = match self.settings.get("acl_phy").unwrap_or("auto") { "1M" => 1, "2M" => 2, _ => 3 };
        match session.configure_acl_phy(handle, acl_phy) {
            Ok(summary) => log(summary),
            Err(error) => log(format!("ACL PHY preference unavailable; keeping negotiated PHY: {error}")),
        }
        let capabilities = match session.read_capabilities(&mut link, |p| report(p)) {
            Ok(capabilities) => capabilities,
            Err(e) => {
                session.disconnect(handle);
                return Err(format!("capability discovery failed: {e}"));
            }
        };

        if let Ok(mut cache)=known_devices().lock() {
            cache.entry(address.to_ascii_uppercase()).and_modify(|d|d.caps=capabilities.clone())
                .or_insert_with(||KnownDevice{caps:capabilities.clone(),qos:Vec::new()});
        }
        emit(json!({
            "event": "capabilities",
            "address": address,
            "summary": describe_capabilities(&capabilities),
            // The union of every PAC record, so the settings page can colour a
            // value the moment it is chosen instead of accepting it and failing
            // on the next connection. A device publishes one record per sample
            // rate, so any single record understates what it can do.
            "sink": codec_envelope(&capabilities.sink_records),
            "source": codec_envelope(&capabilities.source_records),
        }));

        // Whether anyone else has these headphones right now, read the only way
        // the specification provides. A headset busy with a phone otherwise
        // fails somewhere in stream setup with a message about ASEs, which
        // reads as broken hardware rather than as the entirely normal situation
        // it is.
        let availability = olea_core::multipoint::availability(&capabilities);
        log(format!("multipoint: {}", availability.explain()));
        if let Some(contexts) = capabilities.available_contexts {
            log(format!(
                "  available now: {} (supported: {})",
                olea_core::multipoint::describe_contexts(contexts),
                capabilities
                    .supported_contexts
                    .map(olea_core::multipoint::describe_contexts)
                    .unwrap_or_else(|| "not published".into()),
            ));
        }
        emit(json!({
            "event": "availability",
            "address": address,
            "state": availability.as_str(),
            "detail": availability.explain(),
        }));

        if !availability.worth_attempting() {
            session.disconnect(handle);
            return Err(format!("{}", availability.explain()));
        }

        // The raw records, byte for byte. Everything about this device has been
        // read through one parser, and "stereo in one stream: no" - the single
        // fact that sent the whole design down the two-channel path - has never
        // been checked against the bytes it came from. If that reading is wrong,
        // so is every decision built on it.
        for (index, record) in capabilities.sink_records.iter().enumerate() {
            // Only with debug on. These four lines of hex are here to check a
            // parser against the bytes it came from - worth every character when
            // that is the question, and pure noise on the twentieth reconnect of
            // an ordinary evening.
            if !olea_core::trace::is_enabled() {
                break;
            }
            let hex: String = record
                .raw
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ");

            log(format!(
                "  PAC {}: channels {:?}, maximum frames per SDU {} | {hex}",
                index + 1,
                record.capabilities.channel_counts,
                record.capabilities.max_frames_per_sdu
            ));
        }

        let control_point = match find_control_point(&mut link) {
            Ok(control_point) => control_point,
            Err(error) => {
                session.disconnect(handle);
                return Err(error);
            }
        };
        self.capabilities = Some(capabilities);
        self.control_point = Some(control_point);

        match session.attach_volume_control(&mut link) {
            Ok(Some(summary)) => log(summary),
            Ok(None) => log("device does not expose volume control"),
            Err(e) => log(format!("volume control could not be attached: {e}")),
        }

        // Read once here, then updated by notification. Nothing polls it: the
        // device tells us when the level moves, which costs no airtime in
        // between and is the whole reason to subscribe rather than ask.
        match session.attach_batteries(&mut link) {
            Ok(levels) if levels.is_empty() => {
                log("device does not report its battery over GATT")
            }
            // The number goes to the indicator, not to the console. It is a
            // value that belongs on a display: it changes slowly, it is always
            // visible up there, and printing it on every connect only pushed
            // the lines that do need reading further up.
            Ok(levels) => {
                emit(json!({ "event": "battery", "address": address, "levels": levels }));
            }
            Err(e) => log(format!("battery level could not be read: {e}")),
        }

        // Keep the link: configuring the stream and everything after it happens
        // over this same connection, and dropping it here is exactly why the
        // first version of this agent connected successfully and then played
        // nothing at all.
        self.link = Some(link);
        self.hold(address.to_string(), handle);

        emit(json!({
            "event": "connected",
            "address": address,
            "handle": handle,
            "name": friendly_name,
        }));

        // Windows starts the stream as soon as a headset connects, and so do we.
        // If any later ASCS/CIS/audio setup step fails, close the already-live
        // ACL handle before returning. Otherwise the next click/retry races a
        // ghost connection still retained by the controller and headphones.
        let result = self.play();
        if result.is_err() {
            let _ = self.disconnect();
        }
        result
    }

    /// Configures the stream, brings up the isochronous channels and plays.
    ///
    /// Blocks until the audio stops, so the reading thread is the only place
    /// that can interrupt it.
    fn play(&mut self) -> Result<(), String> {
        let mut recovering_since: Option<Instant> = None;
        loop {
            let (_, handle) = self.connected.clone().ok_or("no device is connected")?;
            let mut link = self.link.take().ok_or("connection is not open")?;

            self.yielded = None;
            self.audio_stable = false;
            let result = self.play_on(&mut link, handle);
            if result.is_err() {
                if let Some(session) = self.session.as_mut() {
                    let ases = std::mem::take(&mut self.configured_ases);
                    if let Some(control_point) = self.control_point {
                        session.release_streams(&mut link, control_point, &ases);
                    }
                    session.release_prepared_group();
                }
            }
            // A real ACL loss can arrive during audio teardown or recovery
            // setup, after run_audio has stopped reading events.
            if self.connected.is_some() {
                if let Some(reason) = self.session.as_mut()
                    .and_then(|session| session.take_disconnection_reason(&link, handle)) {
                    let address = self.release_hold().map(|(address, _)| address).unwrap_or_default();
                    self.capabilities = None;
                    self.control_point = None;
                    self.configured_ases.clear();
                    self.lost_reason = Some(reason);
                    emit(json!({ "event": "disconnected", "address": address,
                        "reason": reason, "automatic": true }));
                    return Ok(());
                }
            }
            // A controller-reported disconnect makes this Link permanently stale.
            // Keeping it used to leave both the UI and the next attempt believing a
            // dead ACL connection was still usable.
            if self.connected.is_some() {
                self.link = Some(link);
            }

            let recover_audio = match result {
                Ok(recover) => {
                    if self.audio_stable {
                        recovering_since = None;
                    }
                    recover
                }
                Err(error) if recovering_since.is_some() => {
                    log(format!("audio recovery setup failed: {error}"));
                    true
                }
                Err(error) => return Err(error),
            };
            if recover_audio && self.connected.is_some() {
                let since = *recovering_since.get_or_insert_with(Instant::now);
                if !self.wait_audio_recovery(since)? { return Ok(()); }
                // Re-read settings so changes during recovery are actually used.
                self.refresh_connection_config()?;
                log("rebuilding audio over retained Bluetooth connection");
                continue;
            }

            // Anything other than a deliberate hand-back is the end of playback.
            let Some((sample_rate, frame_us)) = self.yielded.take() else {
                return Ok(());
            };
            if self.connected.is_none() {
                return Ok(());
            }

            // Nothing is configured on the headphones now and nothing is being
            // transmitted. Waiting here is what lets a phone have them, and the
            // moment this PC makes a sound again the loop builds the whole
            // stream back - configure, QoS, enable - which is also the
            // specified way to take a multipoint device over.
            let heard = {
                let playing = self.playing.clone();
                let interrupt = self.interrupt.clone();
                // Raised for the duration of the wait, because the reading
                // thread clears it for Disconnect, adapter-off and quit - and
                // this wait has no other way to hear about any of them. It is
                // cleared again below whichever way the wait ends.
                playing.store(true, std::sync::atomic::Ordering::Relaxed);
                let session = self.session.as_mut().ok_or("adapter is off")?;
                let heard = session
                    .wait_for_sound(handle, sample_rate, frame_us, || {
                        interrupt.load(std::sync::atomic::Ordering::Relaxed)
                            || !playing.load(std::sync::atomic::Ordering::Relaxed)
                    })
                    .map_err(|e| format!("waiting for audio failed: {e}"));
                playing.store(false, std::sync::atomic::Ordering::Relaxed);
                heard?
            };

            if !heard {
                return Ok(());
            }

            log("audio is back: taking the headphones again");
            emit(json!({ "event": "reclaiming" }));
        }
    }

    fn wait_audio_recovery(&mut self, since: Instant) -> Result<bool, String> {
        let waiting = Instant::now();
        emit(json!({ "event": "audio-recovery", "state": "waiting" }));
        loop {
            if self.interrupt.load(std::sync::atomic::Ordering::Relaxed) { return Ok(false); }
            let settings = Settings::load(&settings_path());
            let policy = audio_recovery_policy(&settings);
            if !policy.should_retry(since.elapsed()) {
                emit(json!({ "event": "audio-recovery", "state": "stopped" }));
                return Ok(false);
            }
            let (_, handle) = self.connected.as_ref().ok_or("no device connected")?;
            let lost = match (self.session.as_mut(), self.link.as_mut()) {
                (Some(session), Some(link)) => session.service_recovery_link(link, *handle).map_err(|e| format!("audio recovery interrupted: {e}"))?,
                _ => return Ok(false),
            };
            if let Some(reason) = lost {
                let address = self.release_hold().map(|(a, _)| a).unwrap_or_default();
                self.link = None;
                self.capabilities = None;
                self.control_point = None;
                self.lost_reason = Some(reason);
                emit(json!({ "event": "disconnected", "address": address, "reason": reason, "automatic": true }));
                return Ok(false);
            }
            let interval = policy.interval;
            if waiting.elapsed() >= interval && (!settings.bool("multipoint_yield_enabled").unwrap_or(true)
                || self.session.as_ref().is_some_and(|session| session.media_available())) { return Ok(true); }
        }
    }

    /// Returns true when audio should be rebuilt over the retained ACL.
    fn play_on(&mut self, link: &mut olea_core::Link, handle: u16) -> Result<bool, String> {
        if self.interrupt.load(std::sync::atomic::Ordering::Relaxed) { return Ok(false); }
        let microphone_enabled = self.settings.get("microphone_mode").unwrap_or("off") == "on";

        let capabilities = self
            .capabilities
            .clone()
            .ok_or("device capabilities have not been loaded")?;
        let control_point = self.control_point.ok_or("ASE control point is unknown")?;
        let gaming = self.settings.get("audio_context").unwrap_or("media") == "game";
        let chosen = self.settings.get("preset").unwrap_or("google").to_string();
        let prefer_single = self.session.as_ref().ok_or("adapter is off")?.config().prefer_single_cis;
        let (plan,label) = olea_core::preflight::plan(&self.settings,&capabilities,prefer_single)?;
        let preset=PresetLabel(label);
        log(if microphone_enabled {"headset microphone enabled"} else {"headset microphone disabled"});
        log(if self.settings.bool("monitor_enabled").unwrap_or(false) {
            "monitoring enabled according to settings"
        } else {
            "monitoring disabled; no PC microphone is opened"
        });


        log(format!("ASE control point {control_point:#06x}, preset {}", preset.0));

        // Needed before configuring, to read each ASE's answer afterwards.
        let sink_handles = link.sink_ase_handles().unwrap_or_default();
        let source_handles = if plan.microphone.is_some() {
            link.source_ase_handles().unwrap_or_default()
        } else {
            Vec::new()
        };



        link.subscribe_ascs_responses(control_point).map_err(|e|format!("ASCS response subscription failed: {e}"))?;

        // Printed rather than left in the hex: which ear each stream claims is
        // the one thing that cannot be checked by listening to a log, and
        // getting it wrong sounds like a codec fault rather than a routing one.
        for (index, allocation) in (0..plan.ase_ids.len())
            .map(|i| (i, plan.channel_allocation(i)))
        {
            // Never fatal. This line exists to describe what is about to be
            // sent; failing the whole stream because a diagnostic met a value
            // it did not recognise is worse than printing the number.
            let side = match allocation {
                olea_core::bap::LOCATION_FRONT_LEFT => "left".to_string(),
                olea_core::bap::LOCATION_FRONT_RIGHT => "right".to_string(),
                olea_core::bap::LOCATION_STEREO => "stereo".to_string(),
                olea_core::bap::LOCATION_MONO => "mono".to_string(),
                other => format!("{other:#010x}"),
            };
            log(format!("  ASE {} → {side} ({allocation:#010x})", plan.ase_ids[index]));
        }

        // Recorded before the result is known: the ASEs are configured on the
        // device the moment the writes land, so they need releasing even if
        // everything after this fails.
        self.configured_ases = plan.ase_ids.clone();
        if let Some(microphone) = &plan.microphone {
            self.configured_ases.push(microphone.ase_id);
        }

        // Everything back to Idle first, microphone included. Whatever
        // connected to these headphones before us may have left endpoints
        // configured, and those hold isochronous budget the two streams we
        // actually want are then short of.
        {
            let session = self.session.as_mut().ok_or("adapter is off")?;
            session.release_all_streams(link, control_point, &capabilities);
            // Release may finish in Idle OR Codec Configured (cached codec).
            // Never reconfigure while an endpoint is still releasing/streaming.
            for &(id, value_handle) in sink_handles.iter().chain(source_handles.iter()) {
                wait_for_ase(link, id, value_handle, |value| {
                    match olea_core::bap::ase::parse_state(value) {
                        Some(state) if state.ase_id == id && matches!(state.state,
                            olea_core::bap::ase::STATE_IDLE | olea_core::bap::ase::STATE_CODEC_CONFIGURED) => Ok(()),
                        _ => Err(format!("ASE {id}: previous stream has not been released")),
                    }
                })?;
            }
        }

        // Config Codec first, on its own. The device answers by publishing the
        // QoS it prefers, and that answer is worth reading before committing to
        // a presentation delay it may not be able to meet.
        {
            let session = self.session.as_mut().ok_or("adapter is off")?;
            session
                .write_ascs(link, control_point, &plan.codec_writes())
                .map_err(|e| format!("konfigurace kodeku selhala: {e}"))?;
        }

        let mut plan = plan;
        let selected_sink_handles: Vec<(u8, u16)> = sink_handles
            .iter()
            .copied()
            .filter(|(ase_id, _)| plan.ase_ids.contains(ase_id))
            .collect();
        // Media keeps the device's quality-oriented preferred delay. Gaming
        // asks for the bottom of the range the device itself published; passing
        // Media's 40 ms here made the Low-Latency target largely cosmetic.
        let desired_presentation_delay = if gaming && chosen != "custom" {
            0
        } else {
            plan.qos.presentation_delay_us
        };
        let wanted = ask_device_for_qos(
            link,
            &selected_sink_handles,
            desired_presentation_delay,
            &plan,
            chosen != "custom" && !gaming,
        )?;

        if let Some((address,_))=&self.connected {
            if let Ok(mut cache)=known_devices().lock() {
                if let Some(device)=cache.get_mut(&address.to_ascii_uppercase()) {
                    device.qos.retain(|q|!(q.codec==plan.codec && q.phy==plan.qos.phy && q.target_latency==plan.target_latency && q.context==plan.context));
                    device.qos.push(CachedQos{codec:plan.codec,phy:plan.qos.phy,target_latency:plan.target_latency,context:plan.context,
                        min_delay:wanted.min_delay_us,max_delay:wanted.max_delay_us,max_transport:wanted.max_transport_latency_ms});
                    if device.qos.len()>64 {device.qos.remove(0);}
                }
            }
        }
        if chosen=="custom" && !(wanted.min_delay_us..=wanted.max_delay_us).contains(&plan.qos.presentation_delay_us) {
            return Err(format!("device rejected the custom configuration: presentation delay must be {}–{} ms",wanted.min_delay_us as f64/1000.0,wanted.max_delay_us as f64/1000.0));
        }
        if let Some(delay) = wanted.presentation_delay_us {
            if delay != plan.qos.presentation_delay_us {
                log(format!(
                    "headphones request {} ms delay instead of {} ms; using their value",
                    delay / 1000,
                    plan.qos.presentation_delay_us / 1000
                ));
                plan.qos.presentation_delay_us = delay;
            }
        }

        // The radio numbers, taken from the device for the same reason as the
        // delay: they are what its firmware was tuned around, and the preset's
        // values are what one particular headset was observed to be sent by the
        // Windows driver. On that headset the two agree exactly, so this changes
        // nothing there - and on anything else it is the difference between
        // asking for what the device wants and asking for what a different
        // device wanted.
        //
        // Not applied to the custom preset. There the user is driving, and a
        // control that silently overrides itself is worse than no control.
        if chosen != "custom" {
            if let Some(rtn) = wanted.retransmissions {
                if rtn != plan.qos.retransmission_number {
                    log(format!(
                        "headphones recommend {rtn} retransmissions instead of {}; using theirs",
                        plan.qos.retransmission_number
                    ));
                    plan.qos.retransmission_number = rtn;
                }
            }

            // A ceiling, not a preference: asking for longer than the server
            // supports is a configuration it can only refuse.
            if let Some(limit) = wanted.max_transport_latency_ms {
                if plan.qos.max_transport_latency_ms > limit {
                    log(format!(
                        "headphones support at most {limit} ms transport latency, not {}; using theirs",
                        plan.qos.max_transport_latency_ms
                    ));
                    plan.qos.max_transport_latency_ms = limit;
                }
            }


        }

        if chosen == "custom" {
            if let Some(limit) = wanted.max_transport_latency_ms {
                if plan.qos.max_transport_latency_ms > limit {
                    return Err(format!("custom transport latency exceeds headset limit of {limit} ms"));
                }
            }
        }
        if let Some(mic) = plan.microphone.as_mut() {
            let value_handle = source_handles.iter().find(|(id, _)| *id == mic.ase_id)
                .map(|(_, h)| *h).ok_or("microphone state characteristic missing")?;
            let preferred = wait_for_ase(link, mic.ase_id, value_handle, |value|
                olea_core::negotiation::codec_response(mic.ase_id, &mic.codec, value))?;
            mic.qos.presentation_delay_us = olea_core::negotiation::presentation_delay(
                &[preferred], mic.qos.presentation_delay_us, true)?;
            if preferred.max_transport_latency_ms > 0 {
                mic.qos.max_transport_latency_ms = mic.qos.max_transport_latency_ms.min(preferred.max_transport_latency_ms);
            }
            if preferred.framing != 0 { return Err("microphone requires framed ISO, which is not supported".into()); }
        }

        olea_core::preflight::validate(&plan).map_err(|e|format!("device rejected the custom configuration: {e}"))?;

        // Android/BAP order: controller accepts the group, then the server
        // confirms QoS, then Enable and Create CIS. One transaction in both modes.
        let prepared_cis = self.session.as_mut().ok_or("adapter is off")?
            .prepare_isochronous(&plan, handle)
            .map_err(|e| format!("stream could not be scheduled: {e}"))?;
        let operations = plan.qos_and_enable_writes();
        self.session.as_mut().ok_or("adapter is off")?
            .write_ascs(link, control_point, &operations[..1])
            .map_err(|e| format!("QoS write failed: {e}"))?;
        for (index, &id) in plan.ase_ids.iter().enumerate() {
            let value_handle = selected_sink_handles.iter().find(|(ase, _)| *ase == id)
                .map(|(_, h)| *h).ok_or_else(|| format!("ASE {id}: missing readable characteristic"))?;
            wait_for_ase(link, id, value_handle, |value|
                olea_core::negotiation::qos_response(id, plan.cig_id, index as u8, &plan.qos, value))?;
        }
        if let Some(mic) = &plan.microphone {
            let value_handle = source_handles.iter().find(|(id, _)| *id == mic.ase_id)
                .map(|(_, h)| *h).ok_or("microphone state characteristic missing")?;
            wait_for_ase(link, mic.ase_id, value_handle, |value|
                olea_core::negotiation::qos_response(mic.ase_id, plan.cig_id, mic.cis_id, &mic.qos, value))?;
        }
        log(format!("QoS confirmed on every ASE: {} Hz, {} us frames, {} B/channel, PHY {}, RTN {}, transport ceiling {} ms, presentation {} us",
            plan.codec.sampling_frequency.hz().unwrap_or(0), plan.qos.sdu_interval_us,
            plan.codec.octets_per_frame, plan.qos.phy, plan.qos.retransmission_number,
            plan.qos.max_transport_latency_ms, plan.qos.presentation_delay_us));
        self.session.as_mut().ok_or("adapter is off")?
            .write_ascs(link, control_point, &operations[1..])
            .map_err(|e| format!("Enable failed: {e}"))?;

        // One shape, driven by what this device published, and nothing else.
        //
        // There used to be six: a compatibility profile that walked through
        // other latencies, contexts and PHYs whenever the first attempt did not
        // establish both channels. It cost up to half a minute, and every shape
        // it tried left the headphones configured for something the user never
        // asked for - a game context at 1M, say - so a connection that
        // "succeeded" could sound nothing like the preset on screen. Worse, the
        // winning shape was remembered and used first next time, so the settings
        // page and the actual stream drifted permanently apart.
        //
        // A device that refuses the specification-driven configuration is
        // telling us something real. Retrying the whole connection cleanly is
        // both faster and honest; guessing at parameters is not.
        let cis = {
            let session = self.session.as_mut().ok_or("adapter is off")?;
            match session.establish_prepared(&plan, handle, &prepared_cis) {
                Ok(outcome) if outcome.complete() => {
                    log(format!("radio channels established: {}; checking headphone Streaming state next", outcome.describe()));
                    outcome.established
                }
                Ok(outcome) => {
                    // A partial result is silence with extra steps on a device
                    // that will not start either stream without both, and the
                    // survivors hold the group un-removable. Hand them back so
                    // the next attempt starts from nothing.
                    let survivors = outcome.established.clone();
                    session.release_isochronous(&plan, &survivors);
                    return Err(format!(
                        "headphones did not establish every channel ({}); retrying the connection",
                        outcome.describe()
                    ));
                }
                Err(e) => {
                    session.release_isochronous(&plan, &[]);
                    return Err(format!("isochronous channels could not be established: {e}"));
                }
            }
        };

        // For a Sink ASE the headphones are the Audio Sink. BAP 5.6.3.2 says
        // the server must initiate Receiver Start Ready autonomously once its
        // data path is ready. Sending opcode 0x04 from this client is only valid
        // for a Source ASE (where this client would be the Audio Sink), so do
        // not try to force the transition from the wrong side.
        //
        // More importantly, CAP says to wait for every Sink ASE used by the
        // stream to reach Streaming before sending audio. The old code read the
        // states once, printed them, and sent LC3 regardless. With one ASE still
        // Enabling these headphones render the surviving channel into both ears:
        // precisely "right on both sides, left missing".
        let wanted_ases = selected_sink_handles;
        if wanted_ases.len() != plan.ase_ids.len() {
            return Err(format!(
                "stereo stream cannot be verified: found {} of {} Sink ASE characteristics",
                wanted_ases.len(),
                plan.ase_ids.len()
            ));
        }

        // A Source ASE waits for the client (the audio receiver in this
        // direction) to confirm that its HCI output path is ready.
        if let Some(microphone) = &plan.microphone {
            if !source_handles.iter().any(|(ase_id, _)| *ase_id == microphone.ase_id) {
                return Err(format!(
                    "microphone cannot be verified: Source ASE {} has no readable state characteristic",
                    microphone.ase_id
                ));
            }
            let session = self.session.as_mut().ok_or("adapter is off")?;
            session.start_receivers(link, control_point, &[microphone.ase_id]);
        }
        for (index, &id) in plan.ase_ids.iter().enumerate() {
            let value_handle = wanted_ases.iter().find(|(ase, _)| *ase == id).unwrap().1;
            wait_for_ase(link, id, value_handle, |value|
                olea_core::negotiation::streaming_response(id, plan.cig_id, index as u8, value))?;
            log(format!("ASE {id}: Streaming confirmed, CIG {}, CIS {}, handle {:#06x}, allocation {:#010x}",
                plan.cig_id, index, cis[index], plan.channel_allocation(index)));
        }

        if let Some(microphone) = &plan.microphone {
            let source_handle = source_handles
                .iter()
                .find(|(ase_id, _)| *ase_id == microphone.ase_id)
                .map(|(_, handle)| *handle)
                .ok_or("microphone state is unavailable")?;
            wait_for_ase(link, microphone.ase_id, source_handle, |value|
                olea_core::negotiation::streaming_response(microphone.ase_id, plan.cig_id, microphone.cis_id, value))?;
            log(format!(
                "microphone Source ASE {} active: {} Hz, {} octets per frame",
                microphone.ase_id,
                microphone.codec.sampling_frequency.hz().unwrap_or(0),
                microphone.codec.octets_per_frame
            ));
        }

        emit(json!({ "event": "streaming-started", "cis": cis, "latencyMs": plan.latency_ms(), "configuration": format!("{} Hz · {} ms · {} B/channel · {} CIS · PHY {}M · RTN {} · transport ≤{} ms · presentation {} ms", plan.codec.sampling_frequency.hz().unwrap_or(0), plan.qos.sdu_interval_us as f64 / 1000.0, plan.codec.octets_per_frame, cis.len(), plan.qos.phy, plan.qos.retransmission_number, plan.qos.max_transport_latency_ms, plan.qos.presentation_delay_us as f64 / 1000.0) }));

        if let Some(session) = self.session.as_mut() { session.set_audio_context(plan.context); }
        let playing = self.playing.clone();
        playing.store(true, std::sync::atomic::Ordering::Relaxed);

        let session = self.session.as_mut().ok_or("adapter is off")?;
        let mut lost_reason = None;
        let mut yielded = false;
        let mut stable_audio = false;
        let interrupt = self.interrupt.clone();
        let outcome = session.run_audio(
            &plan,
            &cis,
            Some(handle),
            |progress| {
                match &progress {
                    Progress::Streaming { frames, .. } if frames.saturating_mul(u64::from(plan.qos.sdu_interval_us)) >= 30_000_000 => stable_audio = true,
                    Progress::Disconnected { reason } => lost_reason = Some(*reason),
                    Progress::Yielded { .. } => yielded = true,
                    _ => {}
                }
                report(progress);
            },
            || interrupt.load(std::sync::atomic::Ordering::Relaxed)
                || !playing.load(std::sync::atomic::Ordering::Relaxed),
        );

        playing.store(false, std::sync::atomic::Ordering::Relaxed);
        self.audio_stable = stable_audio;

        // Handing the headphones back is the whole point, so it has to be a
        // real Release rather than simply stopping the audio. An endpoint left
        // configured keeps the headset ours as far as it is concerned, and the
        // phone that wanted it gets nothing.
        //
        // This used to happen only when the headset had asked for them back.
        // Every other way of ending a stream - the user pressing stop, the
        // audio source going away, an error - left the endpoints configured on
        // a device we then walked away from. The headset kept them in Streaming
        // and the next connection opened Config Codec against a state machine
        // that does not allow it from there. The link went down between the
        // configuration and Create CIS, which surfaced as "unknown connection
        // identifier" from the controller: an accurate message about a link
        // that had already gone, and no help at all in finding why.
        //
        // The endpoints are ours to give back however the stream ended - unless
        // the link is already gone, in which case there is nobody to write to
        // and the attempt only spends the control point timeout before failing.
        let ases = std::mem::take(&mut self.configured_ases);
        if !ases.is_empty() && lost_reason.is_none() {
            session.release_streams(link, control_point, &ases);
        }

        if yielded {
            self.yielded = Some((
                plan.codec.sampling_frequency.hz().unwrap_or(48_000),
                plan.qos.sdu_interval_us,
            ));
            log("silence: headphones released, another device may take them");
            emit(json!({ "event": "yielded" }));
        }

        // The channels are still established here - they are what audio was
        // just running over - so they have to be disconnected before the group
        // can be removed. Leaving that to "the next attempt will tidy it up"
        // was wrong: the next attempt removes the group the same way and fails
        // for the same reason, and then asks for a group that cannot be
        // configured while the old one is still there.
        session.release_isochronous(&plan, &cis);
        emit(json!({ "event": "streaming-stopped" }));

        if let Some(reason) = lost_reason {
            let address = self.release_hold().map(|(address, _)| address).unwrap_or_default();
            self.capabilities = None;
            self.control_point = None;
            self.configured_ases.clear();
            self.lost_reason = Some(reason);
            emit(json!({
                "event": "disconnected",
                "address": address,
                "reason": reason,
                "automatic": true,
            }));
        } else if let Err(error) = &outcome {
            // Runtime audio errors are not evidence of an ACL timeout. In
            // particular, CIS loss only requires rebuilding the audio group.
            log(format!("playback failed on ACL {handle:#06x}: {error}"));
            if matches!(error, olea_core::session::SessionError::AdapterSilent | olea_core::session::SessionError::Transport(_) | olea_core::session::SessionError::BatteryReadTimeout) {
                return Err(format!("adapter/transport failure: {error}"));
            }
            let recover = matches!(error,
                olea_core::session::SessionError::AudioDisconnected { .. }
                    | olea_core::session::SessionError::IsoPath(_));
            if !recover {
                emit(json!({ "event": "error", "cmd": "play", "text": format!("Playback stopped: {error}") }));
            }
            return Ok(recover);
        }

        outcome.map(|_| false).map_err(|e| format!("playback ended: {e}"))
    }

    /// Records the device this worker now holds, on both copies.
    fn hold(&mut self, address: String, handle: u16) {
        if let Ok(mut mirror) = self.connected_mirror.write() {
            *mirror = Some(address.clone());
        }
        self.connected = Some((address, handle));
    }

    /// Lets it go, on both copies, and hands back what was held.
    fn release_hold(&mut self) -> Option<(String, u16)> {
        if let Ok(mut mirror) = self.connected_mirror.write() {
            *mirror = None;
        }
        self.connected.take()
    }

    fn disconnect(&mut self) -> Result<(), String> {
        self.playing.store(false, std::sync::atomic::Ordering::Relaxed);

        // Drop our side first, then tell the controller. Dropping the link
        // alone leaves the connection standing as far as the controller is
        // concerned, and every later attempt to reach the same headphones is
        // refused because one already exists.
        let handle = self.release_hold().map(|(_, handle)| handle);

        // Hand the streams back before the link goes: an ASE left in Enabling
        // makes the next connection's configuration an illegal transition, and
        // the device refuses the isochronous channel rather than explaining.
        if let (Some(session), Some(link), Some(control_point)) = (
            self.session.as_mut(),
            self.link.as_mut(),
            self.control_point,
        ) {
            let ases = std::mem::take(&mut self.configured_ases);
            if !ases.is_empty() {
                session.release_streams(link, control_point, &ases);
            }
        }

        self.link = None;
        self.capabilities = None;
        self.control_point = None;

        if let (Some(session), Some(handle)) = (self.session.as_mut(), handle) {
            session.disconnect(handle);
        }

        emit(json!({ "event": "disconnected" }));
        Ok(())
    }

    fn forget(&mut self, command: &Value) -> Result<(), String> {
        let address = command
            .get("address")
            .and_then(Value::as_str)
            .ok_or("address is missing")?;

        // Unpairing the headphones that are playing has to stop them playing.
        // Removing the key on its own left the connection standing with no bond
        // behind it: the row said "not paired" while audio kept coming out, and
        // the key needed to resume it was already gone - so the next drop could
        // only recover by pairing from scratch, which is not what the button
        // said it would do. Done before the removal, because disconnecting
        // cleanly means handing the endpoints back over that same connection.
        let connected_here = self
            .connected
            .as_ref()
            .map(|(connected, _)| connected == address)
            .unwrap_or(false);
        if connected_here {
            log("unpairing the connected device: disconnecting it first");
            let _ = self.disconnect();
        }

        if self.bonds.remove(address) {
            self.bonds
                .save(&BondStore::default_path())
                .map_err(|e| format!("save failed: {e}"))?;
            log(format!("{address} removed"));
        }
        self.status()
    }

    fn report_settings(&mut self) -> Result<(), String> {
        emit_settings_snapshot(&self.settings);
        Ok(())
    }

    fn set(&mut self, command: &Value) -> Result<(), String> {
        let key = command.get("key").and_then(Value::as_str).ok_or("key is missing")?;
        let value = command.get("value").and_then(Value::as_str).ok_or("value is missing")?;
        if command.get("prePersisted").and_then(Value::as_bool) == Some(true) {
            self.settings = Settings::load(&settings_path());
            self.sync_live_audio();
            return Ok(());
        }

        let needs = apply_setting_change(&mut self.settings, key, value)?;
        self.settings.save(&settings_path()).map_err(|e| format!("save failed: {e}"))?;
        self.sync_live_audio();
        emit(json!({ "event": "applied", "key": key, "value": value, "needs": needs }));
        Ok(())
    }

    fn reset_settings(&mut self) -> Result<(), String> {
        self.settings = Settings::defaults();
        self.sync_live_audio();
        self.settings
            .save(&settings_path())
            .map_err(|e| format!("save failed: {e}"))?;
        self.report_settings()
    }
}

/// Reads each configured ASE and returns a presentation delay it will accept.
///
/// Read rather than subscribed: notifications arriving during setup are what
/// stopped the second isochronous channel coming up, and a plain read gets the
/// same value without putting anything extra on the air.
/// What the headphones published about the stream they have just configured.
///
/// Every field is what the device asked for, never what we hoped for. The delay
/// was already being read; the rest was read, printed and then thrown away,
/// which is the worst of both - the log showed the device stating a preference
/// and the stack sending something else.
#[derive(Debug, Default, Clone, Copy)]
struct DevicePreference {
    min_delay_us:u32,
    max_delay_us:u32,
    presentation_delay_us: Option<u32>,
    /// The retransmission count the server recommends.
    retransmissions: Option<u8>,
    /// The longest transport latency the server supports. Ours must not exceed it.
    max_transport_latency_ms: Option<u16>,
    /// Bit 0 is 1M, bit 1 is 2M. Only consulted when 2M is absent.
    phy_preference: Option<u8>,
}

fn wait_for_ase<T>(
    link: &mut olea_core::Link, id: u8, handle: u16,
    verify: impl Fn(&[u8]) -> Result<T, String>,
) -> Result<T, String> {
    link.set_att_timeout(Duration::from_millis(700));
    let deadline = Instant::now() + Duration::from_secs(3);
    let result = (|| {
        loop {
            let value = link.read_characteristic(handle)
                .map_err(|e| format!("ASE {id}: read failed; connection must be rebuilt: {e}"))?;
            link.check_ascs_errors().map_err(|e|e.to_string())?;
            match verify(&value) {
                Ok(value) => return Ok(value),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(40)),
            }
        }
    })();
    link.set_att_timeout(olea_core::link::ATT_TIMEOUT);
    result
}

fn ask_device_for_qos(
    link: &mut olea_core::Link,
    sink_handles: &[(u8, u16)],
    wanted_us: u32,
    plan: &StreamPlan,
    prefer_device: bool,
) -> Result<DevicePreference, String> {
    let mut preference = DevicePreference::default();
    if sink_handles.len() != plan.ase_ids.len() {
        return Err("cannot verify every selected Sink ASE".into());
    }
    let mut preferences = Vec::new();
    for &(ase_id, handle) in sink_handles {
        let index = plan.ase_ids.iter().position(|id| *id == ase_id)
            .ok_or("unexpected Sink ASE")?;
        let mut expected = plan.codec;
        expected.channel_allocation = plan.channel_allocation(index);
        let qos = wait_for_ase(link, ase_id, handle, |value|
            olea_core::negotiation::codec_response(ase_id, &expected, value))?;
        if qos.framing != 0 { return Err(format!("ASE {ase_id} requires framed ISO, which is not supported")); }
        preferences.push(qos);

        log(format!(
            "  ASE {ase_id} requests {}-{} µs delay, prefers {} µs, RTN {}, latency {} ms",
            qos.presentation_delay_min_us,
            qos.presentation_delay_max_us,
            qos.preferred_delay_min_us,
            qos.retransmission_preference,
            qos.max_transport_latency_ms
        ));

        log(format!("  ASE {ase_id} codec verified: {} Hz, {} us, {} octets, allocation {:#010x}",
            expected.sampling_frequency.hz().unwrap_or(0), expected.frame_duration.microseconds(),
            expected.octets_per_frame, expected.channel_allocation));

        // The retransmission count is a recommendation, so the highest any ASE
        // asks for is the one that keeps them all happy. Transport latency is a
        // ceiling, so the lowest is the only one every ASE can meet.
        if qos.retransmission_preference > 0 {
            let rtn = qos.retransmission_preference;
            preference.retransmissions =
                Some(preference.retransmissions.map_or(rtn, |current: u8| current.max(rtn)));
        }
        if qos.max_transport_latency_ms > 0 {
            let latency = qos.max_transport_latency_ms;
            preference.max_transport_latency_ms = Some(
                preference
                    .max_transport_latency_ms
                    .map_or(latency, |current: u16| current.min(latency)),
            );
        }
        if qos.phy_preference != 0 {
            let phy = qos.phy_preference;
            preference.phy_preference =
                Some(preference.phy_preference.map_or(phy, |current: u8| current & phy));
        }
    }

    preference.min_delay_us=preferences.iter().map(|q|q.presentation_delay_min_us).max().unwrap_or(0);
    preference.max_delay_us=preferences.iter().map(|q|q.presentation_delay_max_us).min().unwrap_or(0);
    emit(json!({"event":"qos-limits", "codec":format!("{} Hz / {} ms / {} B / {}M",plan.codec.sampling_frequency.hz().unwrap_or(0),plan.codec.frame_duration.microseconds() as f64/1000.0,plan.codec.octets_per_frame,plan.qos.phy), "minDelayUs":preferences.iter().map(|q|q.presentation_delay_min_us).max(),
        "maxDelayUs":preferences.iter().map(|q|q.presentation_delay_max_us).min(),
        "maxTransportMs":preference.max_transport_latency_ms}));
    preference.presentation_delay_us = Some(olea_core::negotiation::presentation_delay(
        &preferences, wanted_us, prefer_device)?);
    Ok(preference)
}

/// Everything the device said it can accept, flattened into one shape.
///
/// Each PAC record covers one part of what a device supports, and the sum of
/// them is what it will actually take. Reporting the records individually would
/// make the app do this join, and it would then have to know as much about BAP
/// as the stack does.
fn codec_envelope(records: &[olea_core::PacRecord]) -> Value {
    let mut rates: Vec<u32> = Vec::new();
    let mut channels: Vec<u8> = Vec::new();
    let mut frame_ms: Vec<f32> = Vec::new();
    let mut min_octets: Option<u16> = None;
    let mut max_octets: Option<u16> = None;
    let mut max_frames_per_sdu = 0u8;

    for record in records.iter().filter(|record| record.is_lc3()) {
        let caps = &record.capabilities;

        for frequency in &caps.sampling_frequencies {
            if let Some(hz) = frequency.hz() {
                if !rates.contains(&hz) {
                    rates.push(hz);
                }
            }
        }
        for count in &caps.channel_counts {
            if !channels.contains(count) {
                channels.push(*count);
            }
        }
        if caps.supports_7_5ms && !frame_ms.contains(&7.5) {
            frame_ms.push(7.5);
        }
        if caps.supports_10ms && !frame_ms.contains(&10.0) {
            frame_ms.push(10.0);
        }

        // The widest range any record allows. A value outside every record is
        // certain to be refused; one inside a record the device only offers at
        // another sample rate is a maybe, and the app shows those differently.
        if let Some(min) = caps.min_octets_per_frame {
            min_octets = Some(min_octets.map_or(min, |current: u16| current.min(min)));
        }
        if let Some(max) = caps.max_octets_per_frame {
            max_octets = Some(max_octets.map_or(max, |current: u16| current.max(max)));
        }
        max_frames_per_sdu = max_frames_per_sdu.max(caps.max_frames_per_sdu);
    }

    rates.sort_unstable();
    channels.sort_unstable();
    frame_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    json!({
        "rates": rates,
        "frameMs": frame_ms,
        "channels": channels,
        "octetsMin": min_octets,
        "octetsMax": max_octets,
        "maxFramesPerSdu": max_frames_per_sdu,
        "records": records.iter().filter(|record| record.is_lc3()).map(|record| {
            let c = &record.capabilities;
            json!({"rates": c.sampling_frequencies.iter().filter_map(|rate| rate.hz()).collect::<Vec<_>>(),
                "frameMs": ([(7.5, c.supports_7_5ms), (10.0, c.supports_10ms)].into_iter().filter_map(|(ms, yes)| yes.then_some(ms)).collect::<Vec<_>>()),
                "octetsMin": c.min_octets_per_frame, "octetsMax": c.max_octets_per_frame,
                "channels": c.channel_counts, "maxFramesPerSdu": c.max_frames_per_sdu})
        }).collect::<Vec<_>>(),
    })
}

/// A level for the app: null when there is nothing there to measure.
fn level(decibels: f32) -> Option<f32> {
    decibels.is_finite().then(|| (decibels * 10.0).round() / 10.0)
}

/// Locates the ASE control point, the only handle the stream ever writes to.
fn find_control_point(link: &mut olea_core::Link) -> Result<u16, String> {
    use olea_core::link::pacs_uuid;

    let services = link
        .discover_services()
        .map_err(|e| format!("discovery selhalo: {e}"))?;

    let ascs = services
        .iter()
        .find(|s| s.uuid.as_short() == Some(pacs_uuid::SERVICE_ASCS))
        .ok_or("device does not expose the ASCS service")?
        .clone();

    link.discover_characteristics(&ascs)
        .map_err(|e| format!("discovery selhalo: {e}"))?
        .into_iter()
        .find(|c| c.uuid.as_short() == Some(pacs_uuid::ASE_CONTROL_POINT))
        .map(|c| c.value_handle)
        .ok_or_else(|| "ASCS has no control point".to_string())
}

/// The name of whatever produced the plan, preset or not.
struct PresetLabel(String);

/// Reads a preset name saved by version 1.
///
/// Nothing selects these any more - the automatic path is Android's list and
/// the only other choice is custom. Kept so a profile carried over from
/// version 1 can still be read without erroring, and so the names remain
/// searchable when an old log mentions one.
#[allow(dead_code)]
fn parse_preset(name: &str) -> Option<Preset> {
    match name {
        "windows" => Some(Preset::WindowsDefault),
        "low-latency" => Some(Preset::LowLatency),
        "high-quality" => Some(Preset::HighQuality),
        "robust" => Some(Preset::Robust),
        _ => None,
    }
}

/// Forwards a progress report to the app as a line of text.
fn report(progress: Progress) {
    let text = match progress {
        Progress::RadioPower(p) => {
            emit(json!({"event":"radio-power","phy":p.phy,"minDbm":p.min,"adapterMaxDbm":p.adapter_max,
                "currentDbm":p.current,"connectionMaxDbm":p.connection_max,"available":p.available}));return;
        },
        Progress::AdapterReady { version, address } => format!("adapter {version}, {address}"),
        Progress::Connected { handle } => format!("connected, handle {handle:#06x}"),
        // Not logged here. The same text arrives again a moment later on the
        // "capabilities" event, which the app prints, and two identical
        // paragraphs at the top of every connection made the console look like
        // it was retrying something.
        Progress::CapabilitiesRead { summary: _ } => return,
        Progress::StreamPlanned { summary } => summary,
        Progress::Streaming {
            frames,
            backlog,
            iso_failed,
            backpressure_frames,
            iso_sent,
            underruns,
            rssi,
            left_db,
            right_db,
            bass_db,
            mid_db,
            treble_db,
            delivered,
            quality,
            ..
        } => {
            // Cumulative counters straight from the controller. The app turns
            // them into a rate; sending a rate from here would mean deciding the
            // window on its behalf, and the window is a display choice.
            let radio: Vec<Value> = quality
                .iter()
                .map(|q| {
                    json!({
                        "handle": q.handle,
                        "lost": q.lost_packets(),
                        "unacked": q.tx_unacked_packets,
                        "flushed": q.tx_flushed_packets,
                        "retransmitted": q.retransmitted_packets,
                        "crcErrors": q.crc_error_packets,
                        "unreceived": q.rx_unreceived_packets,
                    })
                })
                .collect();

            emit(json!({
                "event": "streaming",
                "frames": frames,
                "backlog": backlog,
                "failed": iso_failed,
                "sent": iso_sent,
                "underruns": underruns,
                "backpressureFrames": backpressure_frames,
                "rssi": rssi,
                "leftDb": level(left_db),
                "rightDb": level(right_db),
                "bassDb": level(bass_db),
                "midDb": level(mid_db),
                "trebleDb": level(treble_db),
                "completed": delivered,
                "radio": radio,
            }));
            return;
        }
        Progress::Battery { levels } => {
            emit(json!({ "event": "battery", "levels": levels }));
            return;
        }
        Progress::BatteryAsked { reason } => format!(
            "battery level requested from the headphones ({reason})"
        ),
        Progress::CaptureReady { device, format } => format!("zdroj zvuku: {device} - {format}"),
        Progress::Idle { after } => format!("silent for {} s, transmission paused", after.as_secs()),
        Progress::Yielded => {
            "the headphones requested release; this may be a multipoint handover".into()
        }
        Progress::Resumed => "audio resumed".into(),
        Progress::LinkState { summary } => summary,
        Progress::Disconnected { reason } => format!(
            "peer disconnected: {} (code {reason:#04x})",
            olea_core::hci::disconnect_reason(reason)
        ),
        Progress::FrameRefused { total } => format!(
            "captured frame looked like noise and was silenced ({total} so far)"
        ),
        Progress::Stopped { reason } => reason,
        Progress::DeviceFound { .. } => return,
    };

    log(text);
}

/// Not used yet, but the app will need it: a deadline the UI can show.
#[allow(dead_code)]
fn remaining(deadline: Instant) -> u64 {
    deadline.saturating_duration_since(Instant::now()).as_secs()
}

#[allow(dead_code)]
fn unused(_: Sender<Value>) {}


#[cfg(test)]
mod settings_tests {
    //! Does choosing a value in the settings panel actually do anything?
    //!
    //! This file had no tests, and the whole suite stayed green while the
    //! Google/Android entry was added to the dropdown and rejected by the code
    //! behind it: the panel reported an error, the codec values on screen kept
    //! describing the old configuration, and the stream carried on unchanged.
    //! Compiling and passing said nothing about it, because nothing exercised
    //! the path between the two.
    //!
    //! So the rule these tests hold is simple: every value the interface can
    //! offer has to be accepted here, and choosing one has to change what will
    //! be sent.

    use super::*;

    /// Exactly the entries MainWindow.xaml.cs puts in the quality dropdown.
    /// When one is added there it has to be added here too, and this test then
    /// proves the code behind it understands the new value.
    const OFFERED_BY_THE_INTERFACE: &[&str] = &["google", "custom"];

    /// Values written by version 1 that can still be sitting in a saved
    /// profile. None of them may error.
    const SAVED_BY_VERSION_ONE: &[&str] = &["windows", "high-quality", "low-latency", "robust"];

    #[test]
    fn every_choice_the_interface_offers_is_accepted() {
        for value in OFFERED_BY_THE_INTERFACE {
            let mut settings = Settings::default();
            apply_setting_change(&mut settings, "preset", value)
                .unwrap_or_else(|e| panic!("the panel offers {value:?} and the agent said: {e}"));
        }
    }

    #[test]
    fn a_profile_from_version_one_still_opens() {
        for value in SAVED_BY_VERSION_ONE {
            let mut settings = Settings::default();
            apply_setting_change(&mut settings, "preset", value)
                .unwrap_or_else(|e| panic!("a saved {value:?} profile failed to load: {e}"));

            // Those presets no longer exist, so the value must not stay behind
            // pointing at one. Anything that is not custom means Android.
            assert_eq!(
                settings.get("preset"),
                Some("google"),
                "{value:?} should have been migrated to the Android path"
            );
        }
    }

    #[test]
    fn choosing_google_puts_androids_first_choice_on_screen() {
        // The bug this test exists for: the panel kept showing 48 kHz / 7.5 ms
        // / 90 octets - the version 1 configuration - after Google/Android had
        // been selected, so there was no way to tell the choice had taken.
        let mut settings = Settings::default();
        settings.set("rate_hz", "48000");
        settings.set("frame_ms", "7.5");
        settings.set("octets", "90");

        apply_setting_change(&mut settings, "preset", "google").expect("google is a valid choice");

        assert_eq!(settings.get("rate_hz"), Some("48000"));
        assert_eq!(settings.get("frame_ms"), Some("10"), "48_4 is a 10 ms setting");
        assert_eq!(settings.get("octets"), Some("120"), "48_4 carries 120 octets");
    }

    #[test]
    fn custom_leaves_the_codec_values_alone() {
        // The other half of the same promise: custom is the user's to set, so
        // selecting it must not overwrite what they typed.
        let mut settings = Settings::default();
        settings.set("rate_hz", "32000");
        settings.set("frame_ms", "7.5");
        settings.set("octets", "80");

        apply_setting_change(&mut settings, "preset", "custom").expect("custom is a valid choice");

        assert_eq!(settings.get("rate_hz"), Some("32000"));
        assert_eq!(settings.get("frame_ms"), Some("7.5"));
        assert_eq!(settings.get("octets"), Some("80"));
        assert_eq!(settings.get("preset"), Some("custom"));
    }

    #[test]
    fn switching_to_google_and_back_to_custom_is_not_a_one_way_door() {
        // Going Google, then Custom, then Google again has to end up where the
        // first Google left it - not stuck on whatever custom was edited to.
        let mut settings = Settings::default();

        apply_setting_change(&mut settings, "preset", "google").unwrap();
        let google_octets = settings.get("octets").map(str::to_string);

        apply_setting_change(&mut settings, "preset", "custom").unwrap();
        apply_setting_change(&mut settings, "octets", "75").unwrap();
        assert_eq!(settings.get("octets"), Some("75"));

        apply_setting_change(&mut settings, "preset", "google").unwrap();
        assert_eq!(settings.get("octets").map(str::to_string), google_octets);
    }

    #[test]
    fn gaming_mode_uses_androids_game_head_and_remains_automatic() {
        let mut settings = Settings::defaults();
        apply_setting_change(&mut settings, "audio_context", "game").unwrap();

        let head = olea_core::google::GAME_ORDER[0].setting;
        assert_eq!(settings.get("preset"), Some("google"));
        assert_eq!(settings.get("audio_context"), Some("game"));
        assert_eq!(
            settings.number("rate_hz").map(|value| value as u32),
            head.sampling_frequency.hz()
        );
        assert_eq!(settings.number("octets").map(|value| value as u16), Some(head.octets_per_frame));
    }

    #[test]
    fn malformed_or_out_of_range_settings_are_rejected_before_save() {
        let mut settings = Settings::defaults();
        assert!(apply_setting_change(&mut settings, "reconnect_interval_s", "0").is_err());
        assert!(apply_setting_change(&mut settings, "octets", "NaN").is_err());
        assert!(apply_setting_change(&mut settings, "audio_context", "unknown").is_err());
        assert!(apply_setting_change(&mut settings, "not_a_setting", "1").is_err());
    }
}

#[cfg(test)]
mod audit_regression_tests {
    use super::*;
    #[test]
    fn transient_encryption_errors_never_invalidate_a_bond() {
        use olea_core::controller::ControllerError;
        use olea_core::session::SessionError;
        for status in [0x05, 0x06] {
            assert!(saved_key_rejected(&SessionError::Controller(ControllerError::EncryptionRejected { status })));
        }
        for status in [0x08, 0x0c, 0x1f] {
            assert!(!saved_key_rejected(&SessionError::Controller(ControllerError::EncryptionRejected { status })));
        }
        assert!(!saved_key_rejected(&SessionError::Controller(ControllerError::EventTimeout)));
        assert!(!saved_key_rejected(&SessionError::EncryptionFailed));
        assert!(!saved_key_rejected(&SessionError::Transport(olea_core::transport::TransportError::Usb("unplugged".into()))));
    }

    #[test]
    fn radio_loss_is_retryable_but_invalid_custom_codec_is_not() {
        assert!(!deterministic_configuration_error("connection failed: connection attempt did not complete"));
        assert!(deterministic_configuration_error("device rejected the custom configuration: unsupported rate"));
    }

}

#[cfg(test)]
mod transport_improvement_tests {
    use super::*;
    #[test]
    fn battery_and_metrics_settings_reach_live_playback() {
        let live = std::sync::Arc::new(std::sync::RwLock::new(LiveAudioConfig::default()));
        let mut settings = Settings::defaults();
        settings.set("battery_poll_min", "0");
        settings.set("link_metrics", "off");
        sync_live_from_settings(&live, &settings);
        assert_eq!(live.read().unwrap().battery_poll_override, Some(None));
        settings.set("battery_poll_min", "5");
        settings.set("link_metrics", "full");
        sync_live_from_settings(&live, &settings);
        assert_eq!(live.read().unwrap().battery_poll_override, Some(Some(Duration::from_secs(300))));
        assert_eq!(live.read().unwrap().metrics_override, Some(MetricsLevel::Full));
    }

}

#[cfg(test)]
mod consent_tests {
    use super::connection_authorized;
    #[test] fn approval_is_consumed_even_when_the_next_attempt_is_a_reconnect() {
        let mut approved=true;
        assert!(connection_authorized(false,&mut approved));
        assert!(!connection_authorized(false,&mut approved));
    }
    #[test] fn removing_a_star_requires_a_new_approval() {
        let mut approved=false;
        assert!(connection_authorized(true,&mut approved));
        assert!(connection_authorized(true,&mut approved));
        assert!(!connection_authorized(false,&mut approved));
    }
}

#[cfg(test)]
mod recovery_policy_tests {
    use super::*;
    #[test]
    fn audio_recovery_is_unlimited_by_default_and_independent_of_acl_reconnect() {
        let mut settings = Settings::defaults();
        settings.set("reconnect_enabled", "false");
        assert!(audio_recovery_policy(&settings).should_retry(Duration::from_secs(86400)));
        apply_setting_change(&mut settings, "audio_recovery_window_min", "2").unwrap();
        assert!(audio_recovery_policy(&settings).should_retry(Duration::from_secs(119)));
        assert!(!audio_recovery_policy(&settings).should_retry(Duration::from_secs(120)));
        apply_setting_change(&mut settings, "audio_recovery_enabled", "false").unwrap();
        assert!(!audio_recovery_policy(&settings).should_retry(Duration::ZERO));
    }
    #[test]
    fn new_settings_are_validated_and_recovery_applies_immediately() {
        let mut settings = Settings::defaults();
        assert!(apply_setting_change(&mut settings, "audio_recovery_interval_s", "0").is_err());
        assert!(apply_setting_change(&mut settings, "audio_recovery_window_min", "NaN").is_err());
        assert!(apply_setting_change(&mut settings, "acl_phy", "Coded").is_err());
        apply_setting_change(&mut settings, "audio_recovery_interval_s", "5").unwrap();
        assert_eq!(audio_recovery_policy(&settings).interval, Duration::from_secs(5));
    }
}
