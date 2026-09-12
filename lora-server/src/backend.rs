use anyhow::Result;
use sx127x::ReceivedPacket;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{Frame, Setting};
use crate::Args;

/// The current competition's ID (see Args::competition_id), shared between
/// the pusher thread (reads it fresh on every push) and the web dashboard
/// (POST /setcompetitionid writes it, GET /status.json reads it) — plain
/// `Arc<Mutex<String>>` rather than routing through the daemon's command
/// channel like most other runtime settings, since nothing in
/// run_daemon_loop's own radio-side logic needs to know it at all; this is
/// purely an HTTP-push-to-roc-server concern.
pub type SharedCompetitionId = Arc<Mutex<String>>;

// ── Delay ─────────────────────────────────────────────────────────────────────

struct StdDelay;

impl embedded_hal::delay::DelayNs for StdDelay {
    fn delay_ns(&mut self, ns: u32) {
        std::thread::sleep(std::time::Duration::from_nanos(ns as u64));
    }
}

// ── Radio trait ───────────────────────────────────────────────────────────────

/// Out-of-band activity the daemon logs (via log_event, into daemon_state)
/// that isn't an incoming radio packet (its own outgoing heartbeats/sends,
/// or errors) — lora-tui's HttpRadio reconstructs these from the polled
/// packet log. Real radio backends have nothing to report here.
#[derive(Debug, PartialEq)]
pub enum StatusEvent {
    Heartbeat { dest: u16 },
    Tx { dest: u16, payload: String },
    Err(String),
    /// A command this daemon originated was confirmed applied by its target.
    CmdOk { target: u16, setting: Setting },
    /// A command this daemon originated got no ack after retrying.
    CmdErr { target: u16, setting: Setting },
    /// A heartbeat was received from another node — the node-health source
    /// for lora-tui's node table (see app.rs), same data daemon_state.rs
    /// tracks for the web dashboard. `si_present` is `None` when the sender
    /// didn't report it (see Heartbeat's own doc comment).
    HeartbeatRx { node: u16, battery: Option<(u8, u16)>, si_present: Option<bool> },
    /// Confirms a TESTPUNCH this client (or another attached client) sent
    /// was actually recorded.
    TestPunchOk { card_id: u32, station: u8, time_s: u32 },
    /// Confirms a CLEARPUNCH this client (or another attached client) sent
    /// actually removed an unsent local punch.
    ClearPunchOk { id: i64 },
    /// Confirms a CLEARPUNCHES this client (or another attached client) sent
    /// removed `count` unsent local punches.
    ClearPunchesOk { count: usize },
    /// A punch this daemon originated (locally, or as an attached client's
    /// TESTPUNCH) was acked by `acked_by` — the node that actually received
    /// and buffered it, not necessarily its final consumer if there's
    /// further relaying upstream of that.
    PunchAckOk { card_id: u32, acked_by: u16 },
    /// This daemon received and applied a Command from `commander` — the
    /// receiving side of a command exchange, distinct from CmdOk (the
    /// originating side, once its own command gets acked back).
    CmdApplied { commander: u16, setting: Setting },
    /// This daemon consumed a punch addressed to it, from `origin` — the
    /// node-health source for lora-tui's/the web dashboard's per-node punch
    /// count (see daemon_state::NodeStatus::punch_count's doc comment on
    /// why this counts packets, not individual station taps).
    PunchRx { origin: u16, card_id: u32 },
    /// A node reported its firmware version — either its unprompted boot
    /// announcement, or a reply to a QUERYVERSION this session (or another
    /// attached client) issued. Same event either way; lora-tui doesn't
    /// need to know which prompted it, just the current value to show in
    /// the node table.
    VersionRx { origin: u16, version: String },
}

pub trait Radio: Send {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<ReceivedPacket>>;

    /// Sends a synthetic test punch through the real buffer/send/ack
    /// pipeline (see the TESTPUNCH command in run_daemon_loop, reachable via
    /// POST /testpunch — web.rs — and unsent_local's doc comment in
    /// punch_buffer.rs). Only meaningful when attached to a running daemon
    /// (HttpRadio) — a direct-hardware session has no punch buffer of its
    /// own to inject into.
    fn send_test_punch(&mut self, _card_id: u32, _station: u8, _time_s: u32) -> Result<()> {
        anyhow::bail!("test punch requires attaching to a running daemon (see lora-tui) — not available in direct hardware mode")
    }

    /// Abandons one stuck unsent local punch (see PunchBuffer::clear_local_unsent).
    /// Same "needs an attached daemon" restriction as send_test_punch — a
    /// direct-hardware session has no punch buffer to clear from.
    fn clear_punch(&mut self, _id: i64) -> Result<()> {
        anyhow::bail!("clear punch requires attaching to a running daemon (see lora-tui) — not available in direct hardware mode")
    }

    /// Abandons every stuck unsent local punch (see PunchBuffer::clear_all_local_unsent).
    fn clear_all_punches(&mut self) -> Result<()> {
        anyhow::bail!("clear punches requires attaching to a running daemon (see lora-tui) — not available in direct hardware mode")
    }

    /// Changes the competition ID the daemon's pusher thread sends with
    /// every future punch push (see backend::SharedCompetitionId and
    /// pusher.rs) — takes effect immediately, no restart needed. Same
    /// "needs an attached daemon" restriction as send_test_punch: a
    /// direct-hardware session has no pusher thread/roc-server relationship
    /// at all to change this for.
    fn set_competition_id(&mut self, _id: &str) -> Result<()> {
        anyhow::bail!("setting competition id requires attaching to a running daemon (see lora-tui) — not available in direct hardware mode")
    }

    fn set_dest(&mut self, _dest: u16) -> Result<()> { Ok(()) }
    fn poll_status(&mut self) -> Vec<StatusEvent> { Vec::new() }

    /// Ask the node at `target` to change its heartbeat interval, as
    /// `commander` (this node's own address). Direct hardware backends send
    /// this as a single, untracked frame — a human watching the TUI can
    /// retry manually if no ack shows up. The daemon's HttpRadio instead
    /// hands this off to the daemon itself (POST /cmd — web.rs), which owns
    /// retry-with-ack tracking (see run_daemon_loop).
    fn send_command(&mut self, commander: u16, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
        let frame = Frame::Command { target, commander, setting: Setting::HeartbeatIntervalSecs(heartbeat_interval_secs) };
        self.send(target, frame.encode().as_bytes())
    }

    /// Ask the node at `target` to report its firmware version. Same
    /// direct-hardware-vs-daemon split as send_command: this default impl
    /// (fire-and-forget, no retry tracking — see the QUERYVERSION handling
    /// in run_daemon_loop for why) is what a direct-hardware session uses;
    /// the daemon's HttpRadio hands it off to the daemon itself instead
    /// (POST /queryversion — web.rs).
    fn query_version(&mut self, target: u16) -> Result<()> {
        let frame = Frame::VersionQuery { target };
        self.send(target, frame.encode().as_bytes())
    }

    /// Generic counterpart to send_command for any `Setting` (see
    /// protocol.rs's module doc comment for which settings this is
    /// actually for — the settings-mode-gated ones — and why). Same
    /// direct-hardware-vs-daemon split: this default impl is fire-and-forget
    /// (a direct-hardware session has no retry-tracked pending_commands of
    /// its own — see run_daemon_loop); the daemon's HttpRadio hands it off
    /// to the daemon itself instead (POST /setcfg — web.rs), which owns
    /// retry-with-ack tracking the same way it already does for CMD.
    fn send_setting(&mut self, commander: u16, target: u16, setting: Setting) -> Result<()> {
        let frame = Frame::Command { target, commander, setting };
        self.send(target, frame.encode().as_bytes())
    }
}

// ── Config builder ────────────────────────────────────────────────────────────

fn build_sx127x_config(args: &Args) -> Result<sx127x::Config> {
    use sx127x::{Bandwidth, CodingRate};

    let bandwidth = Bandwidth::from_hz(args.bw_hz)
        .ok_or_else(|| anyhow::anyhow!("unsupported bandwidth {}Hz", args.bw_hz))?;
    let coding_rate = CodingRate::from_denominator(args.cr)
        .ok_or_else(|| anyhow::anyhow!("unsupported coding rate 4/{} — use 5, 6, 7, or 8", args.cr))?;
    if !(7..=12).contains(&args.sf) {
        anyhow::bail!("unsupported spreading factor {} — use 7-12", args.sf);
    }

    Ok(sx127x::Config {
        freq_hz: args.freq * 1_000_000,
        addr: args.addr,
        spreading_factor: args.sf,
        bandwidth,
        coding_rate,
        sync_word: args.sync_word,
        preamble_len: 8,
        tx_power_dbm: args.power as i8,
        crc_on: true,
    })
}

// sx127x::Sx127xSpi is foreign but Radio is our own trait, so implementing
// it directly is fine — no orphan-rule newtype wrapper needed (that was only
// ever required to avoid overlapping with sx126x's now-removed blanket impl).
impl<SPI, RESET, DELAY, DIO0> Radio for sx127x::Sx127xSpi<SPI, RESET, DELAY, DIO0>
where
    SPI: embedded_hal::spi::SpiDevice + Send,
    RESET: embedded_hal::digital::OutputPin + Send,
    DELAY: embedded_hal::delay::DelayNs + Send,
    DIO0: embedded_hal::digital::InputPin + Send,
{
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
        sx127x::LoraRadio::send(self, dest, payload).map_err(|e| anyhow::anyhow!("{}", e))
    }
    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        sx127x::LoraRadio::receive(self).map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> Result<()> {
    run_spi(args)
}

struct RppalPin(rppal::gpio::OutputPin);

impl embedded_hal::digital::ErrorType for RppalPin {
    type Error = std::convert::Infallible;
}
impl embedded_hal::digital::OutputPin for RppalPin {
    fn set_high(&mut self) -> Result<(), Self::Error> { self.0.set_high(); Ok(()) }
    fn set_low(&mut self) -> Result<(), Self::Error> { self.0.set_low(); Ok(()) }
}

/// Wraps the module's DIO0 pin for --dio0-pin: sx127x polls this directly
/// (a plain GPIO read) instead of IRQ_FLAGS over SPI when waiting for
/// TX/CAD completion — see sx127x::Sx127xSpi::new_with_dio0.
struct RppalInputPin(rppal::gpio::InputPin);

impl embedded_hal::digital::ErrorType for RppalInputPin {
    type Error = std::convert::Infallible;
}
impl embedded_hal::digital::InputPin for RppalInputPin {
    fn is_high(&mut self) -> Result<bool, Self::Error> { Ok(self.0.is_high()) }
    fn is_low(&mut self) -> Result<bool, Self::Error> { Ok(self.0.is_low()) }
}

fn run_spi(args: Args) -> Result<()> {
    use rppal::gpio::Gpio;
    use rppal::spi::{Bus, Mode, SimpleHalSpiDevice, SlaveSelect, Spi};

    // Spawned before any radio hardware is touched, not after — a daemon
    // stuck retrying "SPI module not responding" forever used to be
    // indistinguishable from outside from the process not running at all,
    // since /health wasn't even listening yet during that window.
    let radio_ready: crate::health::RadioReady = Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::health::spawn_server(args.health_listen.clone(), Arc::clone(&radio_ready));
    if let Some(roc_health_url) = args.roc_health_url.clone() {
        crate::health::spawn_checker(roc_health_url, Duration::from_secs(args.health_check_interval_secs));
    }

    let config = build_sx127x_config(&args)?;
    let si_rx = crate::sportident::spawn_si_worker();

    // Boxed to Box<dyn Radio> right here: the DIO0 and no-DIO0 paths build
    // genuinely different concrete Sx127xSpi<..., DIO0> types, which can't
    // both be the return type of one closure without unifying them behind
    // a trait object.
    let build_driver = || -> Result<Box<dyn Radio>> {
        let spi = Spi::new(Bus::Spi0, SlaveSelect::Ss0, 1_000_000, Mode::Mode0)
            .map_err(|e| anyhow::anyhow!("SPI open failed: {}", e))?;
        let spi_device = SimpleHalSpiDevice::new(spi);
        let gpio = Gpio::new()?;
        let reset = RppalPin(gpio.get(args.reset_pin)?.into_output_high());

        if let Some(dio0_pin) = args.dio0_pin {
            let dio0 = RppalInputPin(gpio.get(dio0_pin)?.into_input());
            let mut driver = sx127x::Sx127xSpi::new_with_dio0(spi_device, reset, StdDelay, dio0);
            sx127x::LoraRadio::configure(&mut driver, &config).map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(Box::new(driver))
        } else {
            let mut driver = sx127x::Sx127xSpi::new(spi_device, reset, StdDelay);
            sx127x::LoraRadio::configure(&mut driver, &config).map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(Box::new(driver))
        }
    };

    if std::io::stdout().is_terminal() {
        let driver = build_driver()?;
        radio_ready.store(true, std::sync::atomic::Ordering::SeqCst);
        let port_info = format!(
            "SPI0 CE0  freq: {}Hz  sf: {}  bw: {}Hz{}",
            config.freq_hz, config.spreading_factor, config.bandwidth.hz(),
            if args.dio0_pin.is_some() { "  dio0: wired" } else { "" }
        );
        return crate::ui::run_app(port_info, args.addr, args.dest, driver, args.heartbeat_interval, si_rx);
    }

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
    let state = crate::daemon_state::new_shared();
    let competition_id: SharedCompetitionId = Arc::new(Mutex::new(args.competition_id.clone()));
    crate::web::spawn_server(
        args.web_listen.clone(), args.addr, Arc::clone(&state), Arc::clone(&radio_ready), args.roc_health_url.clone(),
        cmd_tx, Arc::clone(&competition_id),
    );

    let radio: Box<dyn Radio> = loop {
        match build_driver() {
            Ok(driver) => {
                radio_ready.store(true, std::sync::atomic::Ordering::SeqCst);
                log::info!("SX1276 module ready on SPI0 CE0{}", if args.dio0_pin.is_some() { " (DIO0 wired)" } else { "" });
                break driver;
            }
            Err(e) => {
                log::warn!("SPI module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let punch_buffer = setup_punch_pipeline(&args, competition_id)?;
    run_daemon_loop(
        DaemonIdentity { own_addr: args.addr, dest: args.dest, heartbeat_interval: args.heartbeat_interval, relay: args.relay },
        cmd_rx, radio, si_rx, punch_buffer, state,
    )
}

// ── Daemon loop ─────────────────────────────────────────────────────────────

/// Records `msg` in the packet log the web dashboard (and lora-tui, via
/// HttpRadio's polling of GET /status.json) reads — the daemon's only event
/// sink now that lora-tui attaches over HTTP instead of a Unix socket.
fn log_event(state: &crate::daemon_state::SharedState, msg: String) {
    state.lock().unwrap().push_log(msg);
}

/// Opens the persistent punch buffer and, if `--push-to` is configured,
/// starts the background pusher thread. Called once per daemon startup;
/// the returned buffer is fed by run_daemon_loop for both local and remote
/// punches regardless of whether pushing is enabled.
fn setup_punch_pipeline(args: &Args, competition_id: SharedCompetitionId) -> Result<Arc<crate::punch_buffer::PunchBuffer>> {
    if let Some(parent) = std::path::Path::new(&args.punch_db).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(&args.punch_db)?);

    if let Some(push_to) = &args.push_to {
        crate::pusher::spawn(
            Arc::clone(&buffer),
            push_to.clone(),
            Duration::from_secs(args.push_interval_secs),
            competition_id,
        );
        log::info!("punch pusher started, pushing to {}", push_to);
    } else {
        log::info!("no --push-to configured; punches are buffered at {} but not pushed anywhere", args.punch_db);
    }

    Ok(buffer)
}

/// How often an un-acked command is retried, and how many attempts before
/// it's given up on. Kept short since a command's only real cost on failure
/// is airtime — unlike a punch, there's nothing to lose by retrying often.
const CMD_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CMD_MAX_ATTEMPTS: u32 = 5;

struct PendingCommand {
    target: u16,
    setting: Setting,
    sent_at: Instant,
    attempts: u32,
}

/// How often an unacked punch is retried. Unlike commands, there's no give-up
/// count — a punch is real event data, not a settings tweak, so it's held and
/// retried indefinitely rather than dropped after N attempts. Stop-and-wait:
/// only one punch is ever outstanding per node (see docs/protocols/
/// lora_online_control_protocol.md), so the next buffered punch waits for
/// this one to be acked before it's even attempted.
const PUNCH_RETRY_INTERVAL: Duration = Duration::from_secs(5);

struct PendingPunch {
    card_id: u32,
    row_ids: Vec<i64>,
    payload: Vec<u8>,
    sent_at: Instant,
    attempts: u32,
}

// Wire tag byte for the binary HB frame — see sportident.rs's TAG_PUNCH doc
// comment for the full tag registry.
const TAG_HB: u8 = 0x03;
// tag + flags + battery_pct + battery_mv(2) — always this exact length,
// unused fields zeroed, so parsing never has to branch on which of several
// shapes it's looking at (the old text format supported three).
const HB_LEN: usize = 5;
const HB_FLAG_BATTERY_VALID: u8 = 0b001;
const HB_FLAG_SI_KNOWN: u8 = 0b010;
const HB_FLAG_SI_CONNECTED: u8 = 0b100;

/// A parsed heartbeat payload — see `parse_heartbeat`'s doc comment for the
/// wire layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Heartbeat {
    battery: Option<(u8, u16)>,
    /// Whether an SI master is currently connected to the sending node —
    /// only ESP32 punch nodes report this (see esp32-node/src/main.rs's
    /// spawn_si_reader_thread); `None` means the sender didn't say, either
    /// because it's a mains-powered relay with no SI-reader concept at all
    /// (lora-server's own bare heartbeat) or older firmware.
    si_present: Option<bool>,
}

/// Binary wire format: `[tag:1][flags:1][battery_pct:1][battery_mv:2]`
/// (big-endian), always exactly `HB_LEN` bytes — see
/// docs/protocols/lora_online_control_protocol.md, "Wire Format". `flags`
/// bit 0 is whether `battery_pct`/`battery_mv` are meaningful (both zeroed
/// otherwise), bit 1 is whether the sender said anything about SI-master
/// presence at all, and bit 2 (only meaningful if bit 1 is set) is that
/// presence itself. Replaces the old three-shapes text format (bare `HB`,
/// legacy `HB <pct> <mv>`, full `HB <pct-or-"-"> <mv-or-"-"> <0-or-1>`) with
/// one fixed-width shape covering every sender.
pub(crate) fn encode_heartbeat(battery: Option<(u8, u16)>, si_present: Option<bool>) -> Vec<u8> {
    let mut flags = 0u8;
    let (pct, mv) = match battery {
        Some((pct, mv)) => {
            flags |= HB_FLAG_BATTERY_VALID;
            (pct, mv)
        }
        None => (0, 0),
    };
    if let Some(si) = si_present {
        flags |= HB_FLAG_SI_KNOWN;
        if si {
            flags |= HB_FLAG_SI_CONNECTED;
        }
    }
    let mv_bytes = mv.to_be_bytes();
    vec![TAG_HB, flags, pct, mv_bytes[0], mv_bytes[1]]
}

/// Inverse of `encode_heartbeat`. Returns `None` if `bytes` isn't a
/// well-formed HB frame (wrong tag or wrong length) rather than a heartbeat
/// at all — the fixed length means there's no partial/truncated case to
/// handle beyond a flat length check.
fn parse_heartbeat(bytes: &[u8]) -> Option<Heartbeat> {
    if bytes.len() != HB_LEN || bytes[0] != TAG_HB {
        return None;
    }
    let flags = bytes[1];
    let battery = (flags & HB_FLAG_BATTERY_VALID != 0)
        .then(|| (bytes[2], u16::from_be_bytes([bytes[3], bytes[4]])));
    let si_present = (flags & HB_FLAG_SI_KNOWN != 0).then_some(flags & HB_FLAG_SI_CONNECTED != 0);
    Some(Heartbeat { battery, si_present })
}

/// Decodes a binary hot-path payload (PUNCH/PUNCH_ACK/HB) into a
/// human-readable description for logging. The wire format itself is
/// packed binary for airtime efficiency (see docs/protocols/
/// lora_online_control_protocol.md, "Wire Format"), but every log line —
/// journal output and the dashboard/lora-tui traffic log alike — should
/// stay exactly as readable as it was back when these frames were plain
/// text. Falls back to a byte count for anything that doesn't decode as one
/// of the three known shapes, which should only happen for genuinely
/// foreign or corrupt traffic, never for a payload this daemon just built
/// itself to send.
pub(crate) fn describe_payload(bytes: &[u8]) -> String {
    if let Some((origin, dest, readout)) = crate::sportident::CardReadout::parse_payload(bytes) {
        let punches: String = readout.punches.iter()
            .map(|p| format!("{}:{}", p.station, p.time_s))
            .collect::<Vec<_>>()
            .join(",");
        return format!("PUNCH origin={} dest={} card={} [{}]", origin, dest, readout.card_id, punches);
    }
    if let Some((node, card_id)) = crate::protocol::parse_punch_ack(bytes) {
        return format!("PACK node={} card={}", node, card_id);
    }
    if let Some(hb) = parse_heartbeat(bytes) {
        let battery = hb.battery
            .map(|(pct, mv)| format!("{}% {}mV", pct, mv))
            .unwrap_or_else(|| "no battery data".to_string());
        let si = match hb.si_present {
            Some(true) => ", SI master connected",
            Some(false) => ", SI master not connected",
            None => "",
        };
        return format!("HB {}{}", battery, si);
    }
    // Not one of the three binary shapes — most likely one of the plain-text
    // control frames (CMD/ACK/VQUERY/VERSION/CACK), which are valid UTF-8 by
    // construction, so show the text directly rather than a byte count.
    // Only genuinely foreign/corrupt binary ever falls through to the count.
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => format!("<{} bytes>", bytes.len()),
    }
}

/// Lowercase hex encoding — no crate pulled in just for this. Used only to
/// carry an arbitrary byte payload safely through the existing text-based
/// HTTP/`cmd_tx` control plane (`HttpRadio::send`, the `/sendbin` endpoint,
/// and the `SENDBIN` command below) without corrupting it, the way a lossy
/// UTF-8 reinterpretation of a binary PUNCH/PACK/HB payload would. Not
/// related to the actual over-the-air wire format, which stays raw bytes.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Inverse of `to_hex`. `None` on odd length or a non-hex character, rather
/// than panicking on attacker- or bug-supplied input from an HTTP body.
pub(crate) fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// (card_id, row ids covered, binary wire payload) — see next_local_batch.
type PunchBatch = (u32, Vec<i64>, Vec<u8>);

/// The next batch of local, unsent punches to (re)transmit: the oldest
/// unsent local row, plus any immediately-following unsent local rows that
/// share its card_id (reconstructing the original CardReadout's grouping,
/// since punches from one card tap are buffered as consecutive rows sharing
/// one card_id). Returns the row ids covered (to mark sent once acked) and
/// the exact payload to send, rebuilt via `CardReadout::to_payload()` so it
/// matches the wire format precisely. `dest` is embedded in the payload
/// itself as the intended final recipient (see to_payload's doc comment) —
/// this daemon's own currently-configured next hop, which for a leaf node
/// with no relay hops in between is also the final consumer.
fn next_local_batch(buffer: &crate::punch_buffer::PunchBuffer, own_addr: u16, dest: u16) -> Result<Option<PunchBatch>> {
    let unsent = buffer.unsent_local()?;
    let Some(first) = unsent.first() else { return Ok(None) };
    let card_id = first.card_id;
    let batch: Vec<_> = unsent.iter().take_while(|p| p.card_id == card_id).collect();
    let row_ids = batch.iter().map(|p| p.id).collect();
    let punches = batch.iter()
        .map(|p| crate::sportident::ControlPunch { station: p.station, time_s: p.time_s })
        .collect();
    let payload = crate::sportident::CardReadout { card_id, punches }.to_payload(own_addr, dest);
    Ok(Some((card_id, row_ids, payload)))
}

/// Sends a Command frame to the radio-layer address `dest` — this node's
/// own configured next hop, not necessarily `target` directly. That's what
/// makes relaying possible: if `target` isn't in direct range, `dest` is
/// the relay that is, exactly mirroring how uplink punch traffic already
/// routes via each node's own `--dest` rather than straight to the final
/// destination. Commanding a node other than the current `--dest` now needs
/// a `SET_DEST` first, same as `SEND` already requires — this makes `CMD`
/// consistent with the rest of the protocol instead of being the one
/// exception that assumed direct reach.
fn send_command_frame(
    radio: &mut dyn Radio, state: &crate::daemon_state::SharedState,
    dest: u16, target: u16, commander: u16, setting: Setting,
) {
    let frame = Frame::Command { target, commander, setting };
    let payload = frame.encode();
    match radio.send(dest, payload.as_bytes()) {
        Ok(()) => {
            log::info!("CMD to {} (via {}): {}", target, dest, payload);
            log_event(state, format!("TX {} {}", dest, payload));
        }
        Err(e) => {
            log::error!("CMD send failed: {}", e);
            log_event(state, format!("ERR CMD: {}", e));
        }
    }
}

/// Re-transmits `raw_payload` unchanged toward `next_hop` — a relay's only
/// job for traffic that isn't its own: pass it on, not track or retry it at
/// this hop. Overall reliability still comes from the end-to-end stop-and-
/// wait between the original sender and the final consumer; a lost forward
/// just means that sender's own retry resends the punch, which gets
/// forwarded again.
// Takes raw bytes, not `&str` — relays both the binary hot-path frames
// (PUNCH/PACK) and the plain-text control frames (CMD/ACK/VQUERY)
// identically, since a relay just re-transmits whatever bytes it received
// unchanged and doesn't need to understand their content either way. Its
// own log line uses describe_payload rather than assuming the bytes are
// printable text.
fn forward(
    radio: &mut dyn Radio, state: &crate::daemon_state::SharedState,
    next_hop: u16, raw_payload: &[u8],
) {
    match radio.send(next_hop, raw_payload) {
        Ok(()) => {
            let description = describe_payload(raw_payload);
            log::info!("relayed to {}: {}", next_hop, description);
            log_event(state, format!("TX {} {}", next_hop, description));
        }
        Err(e) => {
            log::error!("relay forward failed: {}", e);
            log_event(state, format!("ERR TX: {}", e));
        }
    }
}

/// The daemon's own identity and starting config — as opposed to the
/// runtime handles (radio, sockets, buffers) it operates on.
struct DaemonIdentity {
    own_addr: u16,
    dest: u16,
    heartbeat_interval: u64,
    relay: bool,
}

fn run_daemon_loop(
    identity: DaemonIdentity,
    cmd_rx: std::sync::mpsc::Receiver<String>,
    mut radio: Box<dyn Radio>,
    si_rx: std::sync::mpsc::Receiver<crate::sportident::CardReadout>,
    punch_buffer: Arc<crate::punch_buffer::PunchBuffer>,
    state: crate::daemon_state::SharedState,
) -> Result<()> {
    let DaemonIdentity { own_addr, dest, heartbeat_interval, relay } = identity;
    let mut heartbeat_period = (heartbeat_interval > 0).then(|| Duration::from_secs(heartbeat_interval));
    let mut last_heartbeat = Instant::now();
    let mut dest = dest;
    let mut pending_commands: Vec<PendingCommand> = Vec::new();
    let mut pending_punch: Option<PendingPunch> = None;

    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let Some(n) = cmd.strip_prefix("SET_DEST ") {
                if let Ok(n) = n.trim().parse::<u16>() {
                    log::info!("dest changed to {}", n);
                    dest = n;
                }
            } else if let Some(payload) = cmd.strip_prefix("SEND ") {
                match radio.send(dest, payload.as_bytes()) {
                    Ok(()) => {
                        log::info!("TX to {}: {}", dest, payload);
                        log_event(&state, format!("TX {} {}", dest, payload));
                    }
                    Err(e) => {
                        log::error!("TX failed: {}", e);
                        log_event(&state, format!("ERR TX: {}", e));
                    }
                }
            } else if let Some(hex) = cmd.strip_prefix("SENDBIN ") {
                // Byte-safe counterpart to SEND — see HttpRadio::send and
                // web.rs's /sendbin handler for why this exists: a payload
                // relayed this way can be one of the binary hot-path frames
                // (PUNCH/PACK/HB), which plain text (SEND) can't carry
                // without corruption.
                match from_hex(hex) {
                    Some(bytes) => match radio.send(dest, &bytes) {
                        Ok(()) => {
                            log::info!("TX to {}: {}", dest, describe_payload(&bytes));
                            log_event(&state, format!("TX {} {}", dest, describe_payload(&bytes)));
                        }
                        Err(e) => {
                            log::error!("TX failed: {}", e);
                            log_event(&state, format!("ERR TX: {}", e));
                        }
                    },
                    None => log::error!("SENDBIN command had invalid hex payload"),
                }
            } else if let Some(rest) = cmd.strip_prefix("CMD ") {
                let mut parts = rest.splitn(2, ' ');
                if let (Some(target_str), Some(secs_str)) = (parts.next(), parts.next()) {
                    if let (Ok(target), Ok(secs)) = (target_str.parse::<u16>(), secs_str.parse::<u32>()) {
                        let setting = Setting::HeartbeatIntervalSecs(secs);
                        send_command_frame(radio.as_mut(), &state, dest, target, own_addr, setting);
                        pending_commands.push(PendingCommand { target, setting, sent_at: Instant::now(), attempts: 1 });
                    }
                }
            } else if let Some(rest) = cmd.strip_prefix("SETCFG ") {
                // Generic counterpart to CMD, additive rather than a
                // replacement for it — reuses the same retry/ack tracking
                // for any Setting via Setting::parse's own key=value
                // encoding, rather than needing a hardcoded field per
                // setting the way CMD's own hb_interval shorthand is. See
                // protocol.rs's module doc comment for which settings this
                // is actually for (the settings-mode-gated ones) and why.
                let mut parts = rest.splitn(2, ' ');
                if let (Some(target_str), Some(setting_str)) = (parts.next(), parts.next()) {
                    if let (Ok(target), Some(setting)) = (target_str.parse::<u16>(), Setting::parse(setting_str)) {
                        send_command_frame(radio.as_mut(), &state, dest, target, own_addr, setting);
                        pending_commands.push(PendingCommand { target, setting, sent_at: Instant::now(), attempts: 1 });
                    } else {
                        log::error!("SETCFG command had invalid target or setting: {:?}", rest);
                    }
                }
            } else if let Some(rest) = cmd.strip_prefix("QUERYVERSION ") {
                if let Ok(target) = rest.trim().parse::<u16>() {
                    // Fire-and-forget, unlike CMD's retry-tracked
                    // pending_commands — this is a one-off diagnostic
                    // action, not safety/data-critical config, so an
                    // operator just re-issuing it if nothing comes back is
                    // an acceptable substitute for automatic retry.
                    let frame = Frame::VersionQuery { target };
                    let payload = frame.encode();
                    match radio.send(dest, payload.as_bytes()) {
                        Ok(()) => {
                            log::info!("VQUERY to {} (via {})", target, dest);
                            log_event(&state, format!("TX {} {}", dest, payload));
                        }
                        Err(e) => {
                            log::error!("VQUERY send failed: {}", e);
                            log_event(&state, format!("ERR VQUERY: {}", e));
                        }
                    }
                }
            } else if let Some(rest) = cmd.strip_prefix("TESTPUNCH ") {
                // Recorded as "test", not "local" — deliberately flows
                // through the exact same buffer/send/retry/ack pipeline as
                // a genuine local punch (see unsent_local's doc comment in
                // punch_buffer.rs), but stays identifiable as synthetic
                // rather than indistinguishable from a real card tap.
                let mut parts = rest.splitn(3, ' ');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(card_str), Some(station_str), Some(time_str)) => {
                        match (card_str.parse::<u32>(), station_str.parse::<u8>(), time_str.parse::<u32>()) {
                            (Ok(card_id), Ok(station), Ok(time_s)) => {
                                match punch_buffer.record(card_id, station, time_s, "test") {
                                    Ok(_) => {
                                        log::info!("test punch recorded: card {} station {} time {}", card_id, station, time_s);
                                        log_event(&state, format!("TESTPUNCHOK {} {} {}", card_id, station, time_s));
                                    }
                                    Err(e) => {
                                        log::error!("failed to record test punch: {}", e);
                                        log_event(&state, format!("ERR TESTPUNCH: {}", e));
                                    }
                                }
                            }
                            _ => log_event(&state, "ERR TESTPUNCH: bad numeric fields".to_string()),
                        }
                    }
                    _ => log_event(&state, "ERR TESTPUNCH: usage TESTPUNCH <card_id> <station> <time_s>".to_string()),
                }
            } else if let Some(rest) = cmd.strip_prefix("CLEARPUNCH ") {
                match rest.trim().parse::<i64>() {
                    Ok(id) => match punch_buffer.clear_local_unsent(id) {
                        Ok(true) => {
                            // The row is gone from the DB, but if it was the
                            // batch currently being retried in memory, that
                            // in-memory state doesn't know that — without
                            // this, run_daemon_loop would keep retrying a
                            // payload whose backing row(s) no longer exist,
                            // forever, since nothing else ever clears it.
                            if pending_punch.as_ref().is_some_and(|p| p.row_ids.contains(&id)) {
                                pending_punch = None;
                            }
                            log::info!("cleared unsent local punch {id}");
                            log_event(&state, format!("CLEARPUNCHOK {id}"));
                        }
                        Ok(false) => log_event(&state, format!("ERR CLEARPUNCH: no unsent local punch {id}")),
                        Err(e) => log_event(&state, format!("ERR CLEARPUNCH: {}", e)),
                    },
                    Err(_) => log_event(&state, "ERR CLEARPUNCH: usage CLEARPUNCH <id>".to_string()),
                }
            } else if cmd.trim() == "CLEARPUNCHES" {
                match punch_buffer.clear_all_local_unsent() {
                    Ok(n) => {
                        // Whatever was in flight is now definitely gone too.
                        pending_punch = None;
                        log::info!("cleared {n} unsent local punch(es)");
                        log_event(&state, format!("CLEARPUNCHESOK {n}"));
                    }
                    Err(e) => log_event(&state, format!("ERR CLEARPUNCHES: {}", e)),
                }
            }
        }

        pending_commands.retain_mut(|cmd| {
            if cmd.sent_at.elapsed() < CMD_RETRY_INTERVAL {
                return true;
            }
            if cmd.attempts >= CMD_MAX_ATTEMPTS {
                log::warn!("CMD to {} ({:?}) gave up after {} attempts", cmd.target, cmd.setting, cmd.attempts);
                log_event(&state, format!("CMDERR {} {}", cmd.target, cmd.setting.encode()));
                return false;
            }
            cmd.attempts += 1;
            cmd.sent_at = Instant::now();
            send_command_frame(radio.as_mut(), &state, dest, cmd.target, own_addr, cmd.setting);
            true
        });

        if let Some(period) = heartbeat_period {
            if last_heartbeat.elapsed() >= period {
                last_heartbeat = Instant::now();
                if dest == own_addr {
                    // Sending a heartbeat to ourselves is meaningless —
                    // dest never actually filters radio reception (LoRa is
                    // a broadcast medium; see CardReadout::to_payload's doc
                    // comment on why `dest` exists at all), so this isn't
                    // about anyone failing to hear it. It's just burning
                    // airtime and log noise announcing our own liveness to
                    // ourselves — easy to end up here by accident once a
                    // node's own address happens to match its configured
                    // dest (e.g. a base station whose dest was set before
                    // it existed, pointing at the address it'd eventually
                    // be assigned).
                } else {
                    match radio.send(dest, b"HB") {
                        Ok(()) => {
                            log::info!("HB sent to {}", dest);
                            log_event(&state, format!("HB {}", dest));
                        }
                        Err(e) => {
                            log::error!("HB send failed: {}", e);
                            log_event(&state, format!("ERR HB: {}", e));
                        }
                    }
                }
            }
        }

        while let Ok(readout) = si_rx.try_recv() {
            // Buffer immediately, unconditionally — every punch a SportIdent
            // master hands us is safe on disk before we ever try to send it.
            // Actual transmission (and its stop-and-wait retry) happens below,
            // driven by the buffer itself rather than sent inline here.
            for p in &readout.punches {
                if let Err(e) = punch_buffer.record(readout.card_id, p.station, p.time_s, "local") {
                    log::error!("failed to buffer local punch: {}", e);
                }
            }
            log::info!("buffered {} punch(es) for card {}", readout.punches.len(), readout.card_id);
        }

        if dest == own_addr {
            // Radio-transmitting a punch toward ourselves is exactly as
            // meaningless as the self-heartbeat skip above, and for the
            // same reason — nothing to send toward if we're also the
            // destination. Left buffered (sent=0) either way: still picked
            // up by the HTTP pusher if --push-to is configured, or just
            // waiting for `dest` to actually point somewhere. Without this,
            // next_local_batch would keep re-attempting (and PUNCH_RETRY_INTERVAL
            // would keep retrying any already-picked-up batch) forever,
            // logging a stream of pointless send attempts.
        } else if pending_punch.is_none() {
            match next_local_batch(&punch_buffer, own_addr, dest) {
                Ok(Some((card_id, row_ids, payload))) => {
                    match radio.send(dest, &payload) {
                        Ok(()) => {
                            log::info!("PUNCH to {}: {}", dest, describe_payload(&payload));
                            log_event(&state, format!("TX {} {}", dest, describe_payload(&payload)));
                            pending_punch = Some(PendingPunch { card_id, row_ids, payload, sent_at: Instant::now(), attempts: 1 });
                        }
                        Err(e) => {
                            log::error!("PUNCH send failed: {}", e);
                            log_event(&state, format!("ERR TX: {}", e));
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => log::error!("failed to query punch buffer: {}", e),
            }
        } else if let Some(p) = &mut pending_punch {
            if p.sent_at.elapsed() >= PUNCH_RETRY_INTERVAL {
                p.attempts += 1;
                p.sent_at = Instant::now();
                match radio.send(dest, &p.payload) {
                    Ok(()) => {
                        log::info!("PUNCH retry #{} to {}: {}", p.attempts, dest, describe_payload(&p.payload));
                        log_event(&state, format!("TX {} {}", dest, describe_payload(&p.payload)));
                    }
                    Err(e) => {
                        log::error!("PUNCH retry failed: {}", e);
                        log_event(&state, format!("ERR TX: {}", e));
                    }
                }
            }
        }

        match radio.receive() {
            Ok(Some(pkt)) => {
                let rssi_str = pkt.rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".to_string());
                let rssi_suffix = pkt.rssi.map(|dbm| format!(" (RSSI: {}dBm)", dbm)).unwrap_or_default();

                // Binary hot-path frames (PUNCH/PACK/HB) are checked
                // directly against the raw bytes, before any UTF-8
                // conversion — see sportident.rs's TAG_PUNCH doc comment and
                // docs/protocols/lora_online_control_protocol.md's "Wire
                // Format": these payloads are arbitrary binary, and
                // String::from_utf8_lossy would silently corrupt them
                // (substituting invalid sequences) rather than fail
                // cleanly. Only once none of the three binary shapes match
                // do we fall back to decoding text, for the remaining (rare,
                // genuinely textual) control frames. `payload_display` is
                // always a human-readable description regardless of which
                // branch runs — see describe_payload's doc comment for why
                // this matters even though the wire bytes themselves aren't
                // readable anymore.
                let payload_display;
                if let Some((origin, punch_dest, readout)) = crate::sportident::CardReadout::parse_payload(&pkt.payload) {
                    payload_display = describe_payload(&pkt.payload);
                    log::info!("RX from {}: {}{}", pkt.src_addr, payload_display, rssi_suffix);
                    // LoRa is a broadcast medium — every node in radio range
                    // decodes every packet regardless of what `dest` its
                    // sender used (see CardReadout::to_payload's doc
                    // comment), so this check is the only thing that stops
                    // an uninvolved node from also consuming/acking traffic
                    // meant for someone else. The RX log line above already
                    // gives full visibility into everything overheard.
                    if punch_dest == own_addr {
                        state.lock().unwrap().record_punch(origin, pkt.rssi);
                        log_event(&state, format!("PUNCHRX {} {}", origin, readout.card_id));
                        for p in &readout.punches {
                            if let Err(e) = punch_buffer.record(readout.card_id, p.station, p.time_s, &origin.to_string()) {
                                log::error!("failed to buffer remote punch: {}", e);
                            }
                        }
                        // Ack names the true origin (so a relay downstream
                        // knows who it's ultimately for) but is transmitted
                        // to whoever handed us this packet — retracing the
                        // same path backward, one hop at a time. Best-effort,
                        // not itself retried: if it's lost, the sender's own
                        // retry timeout resends the punch, prompting another
                        // ack attempt — self-healing without needing the ack
                        // path itself to be reliable.
                        let ack = crate::protocol::encode_punch_ack(origin, readout.card_id);
                        if let Err(e) = radio.send(pkt.src_addr, &ack) {
                            log::error!("failed to ack punch: {}", e);
                        }
                    } else if relay {
                        // Not ours to consume — pass it on toward our own
                        // next hop unchanged. `punch_dest` (the ORIGINAL
                        // final destination, not us) travels along with it
                        // untouched, so whoever ends up consuming it still
                        // checks against the right address.
                        forward(radio.as_mut(), &state, dest, &pkt.payload);
                    }
                } else if let Some((node, card_id)) = crate::protocol::parse_punch_ack(&pkt.payload) {
                    payload_display = describe_payload(&pkt.payload);
                    log::info!("RX from {}: {}{}", pkt.src_addr, payload_display, rssi_suffix);
                    if node == own_addr {
                        if pending_punch.as_ref().is_some_and(|p| p.card_id == card_id) {
                            let p = pending_punch.take().unwrap();
                            for id in &p.row_ids {
                                if let Err(e) = punch_buffer.mark_sent(*id) {
                                    log::error!("failed to mark punch {} sent: {}", id, e);
                                }
                            }
                            log::info!("PUNCH card {} acked by {}", card_id, pkt.src_addr);
                            log_event(&state, format!("PACKOK {} {}", card_id, pkt.src_addr));
                        }
                    } else if relay {
                        // Named node isn't us — pass the ack on toward it
                        // directly (single-hop relay: assumed within direct
                        // reach downstream of this relay).
                        forward(radio.as_mut(), &state, node, &pkt.payload);
                    }
                } else if let Some(hb) = parse_heartbeat(&pkt.payload) {
                    payload_display = describe_payload(&pkt.payload);
                    log::info!("RX from {}: {}{}", pkt.src_addr, payload_display, rssi_suffix);
                    // Heartbeats don't carry an origin field yet (they can't
                    // relay — see docs/protocols/lora_online_control_protocol.md),
                    // so pkt.src_addr is always the true origin here, unlike
                    // the punch/command branches which use an explicit
                    // `origin`/`commander` field for exactly this reason.
                    state.lock().unwrap().record_heartbeat(pkt.src_addr, hb.battery, hb.si_present);
                    let battery_field = hb.battery
                        .map(|(pct, mv)| format!("{} {}", pct, mv))
                        .unwrap_or_else(|| "-".to_string());
                    let si_field = match hb.si_present {
                        Some(true) => "1",
                        Some(false) => "0",
                        None => "-",
                    };
                    log_event(&state, format!("HBRX {} {} {}", pkt.src_addr, battery_field, si_field));
                } else {
                    // None of the binary hot-path shapes matched — only now
                    // is it safe to assume text, for the remaining plain-text
                    // control frames (CMD/ACK/VQUERY/VERSION/CACK). A lossy
                    // decode here is fine: this daemon never sends any of
                    // those frames as anything but valid UTF-8, so lossiness
                    // only ever shows up on genuinely foreign/corrupt input,
                    // which Frame::parse then correctly rejects anyway.
                    let payload = String::from_utf8_lossy(&pkt.payload).into_owned();
                    payload_display = payload.clone();
                    log::info!("RX from {}: {}{}", pkt.src_addr, payload, rssi_suffix);
                    if let Some(frame) = Frame::parse(&payload) {
                    match frame {
                        Frame::Command { target, commander, setting } if target == own_addr => {
                            // Only HeartbeatIntervalSecs is actually
                            // applicable to this daemon itself — the
                            // settings-mode-gated fields (and TxPowerDbm)
                            // exist for esp32-node targets, since
                            // lora-server's own radio parameters are
                            // commissioning-time CLI args, not runtime-
                            // reconfigurable. No ack for anything
                            // unsupported, same as any other out-of-scope
                            // command — the commander's own retry/no-ack
                            // display is the signal that this daemon isn't
                            // the right target for that setting.
                            let applied = match setting {
                                Setting::HeartbeatIntervalSecs(secs) => {
                                    heartbeat_period = (secs > 0).then(|| Duration::from_secs(secs as u64));
                                    log::info!("heartbeat interval changed to {}s by command from {}", secs, pkt.src_addr);
                                    true
                                }
                                _ => {
                                    log::warn!("ignoring unsupported setting {:?} from {}", setting, pkt.src_addr);
                                    false
                                }
                            };
                            if applied {
                                // Ack names the true commander (so a relay
                                // downstream of it knows where to forward this)
                                // but is transmitted to whoever handed us this
                                // packet, same pattern as the binary punch ack.
                                let ack = Frame::Ack { origin: own_addr, commander, setting };
                                if let Err(e) = radio.send(pkt.src_addr, ack.encode().as_bytes()) {
                                    log::error!("failed to ack command: {}", e);
                                }
                                log_event(&state, format!("CMDAPPLIED {} {}", commander, setting.encode()));
                            }
                        }
                        Frame::Command { target, .. } if relay => {
                            // Not addressed to us — pass it on toward the
                            // named target directly (single-hop relay:
                            // assumed within direct reach downstream).
                            forward(radio.as_mut(), &state, target, payload.as_bytes());
                        }
                        Frame::Command { .. } => {
                            // Not addressed to us and not relaying — ignore.
                        }
                        Frame::Ack { origin, commander, setting } if commander == own_addr => {
                            let had = pending_commands.len();
                            pending_commands.retain(|p| !(p.target == origin && p.setting == setting));
                            if pending_commands.len() < had {
                                log::info!("CMD to {} ({:?}) acked", origin, setting);
                                log_event(&state, format!("CMDOK {} {}", origin, setting.encode()));
                            }
                        }
                        Frame::Ack { commander, .. } if relay => {
                            // Not for us — forward toward the commander who
                            // originally issued this command.
                            forward(radio.as_mut(), &state, commander, payload.as_bytes());
                        }
                        Frame::Ack { .. } => {
                            // Not relaying and not ours — ignore.
                        }
                        Frame::VersionQuery { target } if target == own_addr => {
                            let report = Frame::VersionReport { origin: own_addr, version: crate::version::VERSION.to_string() };
                            let report_payload = report.encode();
                            match radio.send(pkt.src_addr, report_payload.as_bytes()) {
                                Ok(()) => log_event(&state, format!("TX {} {}", pkt.src_addr, report_payload)),
                                Err(e) => log::error!("failed to reply to version query: {}", e),
                            }
                            log_event(&state, format!("VQUERYRX {}", pkt.src_addr));
                        }
                        Frame::VersionQuery { target } if relay => {
                            // Not addressed to us — pass it on toward the
                            // named target directly, same pattern as Command.
                            forward(radio.as_mut(), &state, target, payload.as_bytes());
                        }
                        Frame::VersionQuery { .. } => {
                            // Not addressed to us and not relaying — ignore.
                        }
                        Frame::VersionReport { origin, version } => {
                            // No addressing/relay to check — same accepted
                            // limitation as HB (see Frame::VersionReport's
                            // doc comment): recorded from whoever overheard
                            // it, exactly like battery/SI-master state
                            // already is from any overheard HB.
                            state.lock().unwrap().record_version(origin, version.clone());
                            log_event(&state, format!("VERSIONRX {} {}", origin, version));

                            // Doubles as this base station's answer to "is
                            // node `origin`'s current LoRa mode actually
                            // reaching me?" — see Frame::ConfigAck's doc
                            // comment. Sent unconditionally for every
                            // VersionReport, whether it's a reply to our own
                            // VQUERY or the node's unprompted boot
                            // announcement; harmless either way; simpler
                            // than distinguishing the two.
                            let ack = Frame::ConfigAck { target: origin };
                            let ack_payload = ack.encode();
                            match radio.send(pkt.src_addr, ack_payload.as_bytes()) {
                                Ok(()) => log_event(&state, format!("TX {} {}", pkt.src_addr, ack_payload)),
                                Err(e) => log::error!("failed to send config ack: {}", e),
                            }
                        }
                        Frame::ConfigAck { .. } => {
                            // Only ever sent downlink by lora-server itself
                            // (to a node) — nothing here needs to react to
                            // overhearing one.
                        }
                    }
                    }
                }
                log_event(&state, format!("RX {} {} {}", pkt.src_addr, rssi_str, payload_display));
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("RX error: {}", e);
                log_event(&state, format!("ERR RX: {}", e));
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── Attach mode ───────────────────────────────────────────────────────────────

fn parse_rx_line(line: &str) -> Option<ReceivedPacket> {
    let rest = line.strip_prefix("RX ")?;
    let mut parts = rest.splitn(3, ' ');
    let src: u16 = parts.next()?.parse().ok()?;
    let rssi_str = parts.next()?;
    let rssi: Option<i16> = if rssi_str == "-" { None } else { rssi_str.parse().ok() };
    let text = parts.next().unwrap_or("");
    let mut payload = heapless::Vec::<u8, 240>::new();
    let _ = payload.extend_from_slice(text.as_bytes());
    Some(ReceivedPacket { src_addr: src, payload, rssi })
}

fn parse_status_line(line: &str) -> Option<StatusEvent> {
    if let Some(rest) = line.strip_prefix("HB ") {
        let dest: u16 = rest.trim().parse().ok()?;
        return Some(StatusEvent::Heartbeat { dest });
    }
    if let Some(rest) = line.strip_prefix("TX ") {
        let mut parts = rest.splitn(2, ' ');
        let dest: u16 = parts.next()?.parse().ok()?;
        let payload = parts.next().unwrap_or("").to_string();
        return Some(StatusEvent::Tx { dest, payload });
    }
    if let Some(rest) = line.strip_prefix("ERR ") {
        return Some(StatusEvent::Err(rest.to_string()));
    }
    if let Some(rest) = line.strip_prefix("CMDOK ") {
        let mut parts = rest.splitn(2, ' ');
        let target: u16 = parts.next()?.parse().ok()?;
        let setting = Setting::parse(parts.next()?)?;
        return Some(StatusEvent::CmdOk { target, setting });
    }
    if let Some(rest) = line.strip_prefix("CMDERR ") {
        let mut parts = rest.splitn(2, ' ');
        let target: u16 = parts.next()?.parse().ok()?;
        let setting = Setting::parse(parts.next()?)?;
        return Some(StatusEvent::CmdErr { target, setting });
    }
    if let Some(rest) = line.strip_prefix("HBRX ") {
        // "<node> - <si>" (battery absent) or "<node> <pct> <mv> <si>"
        // (battery present) — token count tells the two apart, since the
        // battery field is a single "-" or a "<pct> <mv>" pair.
        let tokens: Vec<&str> = rest.split(' ').collect();
        let node: u16 = tokens.first()?.parse().ok()?;
        let (battery, si_str) = match tokens.as_slice() {
            [_, "-", si] => (None, *si),
            [_, pct, mv, si] => (Some((pct.parse().ok()?, mv.parse().ok()?)), *si),
            _ => return None,
        };
        let si_present = match si_str {
            "1" => Some(true),
            "0" => Some(false),
            "-" => None,
            _ => return None,
        };
        return Some(StatusEvent::HeartbeatRx { node, battery, si_present });
    }
    if let Some(rest) = line.strip_prefix("TESTPUNCHOK ") {
        let mut parts = rest.splitn(3, ' ');
        let card_id: u32 = parts.next()?.parse().ok()?;
        let station: u8 = parts.next()?.parse().ok()?;
        let time_s: u32 = parts.next()?.parse().ok()?;
        return Some(StatusEvent::TestPunchOk { card_id, station, time_s });
    }
    if let Some(rest) = line.strip_prefix("CLEARPUNCHOK ") {
        let id: i64 = rest.trim().parse().ok()?;
        return Some(StatusEvent::ClearPunchOk { id });
    }
    if let Some(rest) = line.strip_prefix("CLEARPUNCHESOK ") {
        let count: usize = rest.trim().parse().ok()?;
        return Some(StatusEvent::ClearPunchesOk { count });
    }
    if let Some(rest) = line.strip_prefix("PACKOK ") {
        let mut parts = rest.splitn(2, ' ');
        let card_id: u32 = parts.next()?.parse().ok()?;
        let acked_by: u16 = parts.next()?.parse().ok()?;
        return Some(StatusEvent::PunchAckOk { card_id, acked_by });
    }
    if let Some(rest) = line.strip_prefix("CMDAPPLIED ") {
        let mut parts = rest.splitn(2, ' ');
        let commander: u16 = parts.next()?.parse().ok()?;
        let setting = Setting::parse(parts.next()?)?;
        return Some(StatusEvent::CmdApplied { commander, setting });
    }
    if let Some(rest) = line.strip_prefix("PUNCHRX ") {
        let mut parts = rest.splitn(2, ' ');
        let origin: u16 = parts.next()?.parse().ok()?;
        let card_id: u32 = parts.next()?.parse().ok()?;
        return Some(StatusEvent::PunchRx { origin, card_id });
    }
    if let Some(rest) = line.strip_prefix("VERSIONRX ") {
        let mut parts = rest.splitn(2, ' ');
        let origin: u16 = parts.next()?.parse().ok()?;
        let version = parts.next()?.to_string();
        return Some(StatusEvent::VersionRx { origin, version });
    }
    None
}

/// How often HttpRadio polls /status.json for new log lines. Short enough
/// that lora-tui feels live; long enough not to hammer a Pi-hosted daemon
/// from a terminal a human is just watching.
const HTTP_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(serde::Deserialize)]
struct HttpLogLine {
    seq: u64,
    line: String,
}

#[derive(serde::Deserialize)]
struct HttpStatusResponse {
    own_addr: u16,
    log: Vec<HttpLogLine>,
}

/// lora-tui's sole transport when attaching to a running daemon — reads GET
/// /status.json (see web.rs), reusing parse_rx_line/parse_status_line since
/// /status.json's log lines are the exact same text log_event() writes to
/// daemon_state. Commands go out as POST /send, /setdest, /cmd, /testpunch,
/// which web.rs forwards to the daemon's own command channel (cmd_tx).
struct HttpRadio {
    base_url: String,
    /// The attached daemon's own LoRa address, discovered from its first
    /// /status.json response rather than trusted from a CLI flag — see
    /// attach()'s doc comment for why lora-tui no longer takes its own
    /// --addr at all.
    own_addr: u16,
    events: std::sync::mpsc::Receiver<ReceivedPacket>,
    status_events: std::sync::mpsc::Receiver<StatusEvent>,
}

impl HttpRadio {
    fn new(base_url: String) -> Result<Self> {
        let status_url = format!("{}/status.json", base_url);
        let initial: HttpStatusResponse = ureq::get(&status_url)
            .call()
            .map_err(|e| anyhow::anyhow!("cannot reach lora-server at {}: {}", base_url, e))?
            .into_json()
            .map_err(|e| anyhow::anyhow!("bad /status.json from {}: {}", base_url, e))?;

        // log is newest-first (see web.rs::build_status) — start from the
        // highest seq already present so a freshly attached client doesn't
        // replay the whole history, only ever showing live traffic from the
        // point it attaches (see app.rs's NodeStatus doc comment).
        let mut last_seq = initial.log.first().map(|e| e.seq).unwrap_or(0);
        let own_addr = initial.own_addr;

        let (tx, rx) = std::sync::mpsc::channel();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let poll_url = status_url.clone();

        std::thread::spawn(move || loop {
            std::thread::sleep(HTTP_POLL_INTERVAL);

            let resp = match ureq::get(&poll_url).call() {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("status poll of {} failed: {}", poll_url, e);
                    continue;
                }
            };
            let parsed: HttpStatusResponse = match resp.into_json() {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("bad /status.json from {}: {}", poll_url, e);
                    continue;
                }
            };

            let mut new_entries: Vec<&HttpLogLine> = parsed.log.iter().filter(|e| e.seq > last_seq).collect();
            if new_entries.is_empty() {
                continue;
            }
            new_entries.sort_by_key(|e| e.seq); // oldest-of-the-new-batch first, preserving event order

            for entry in &new_entries {
                if let Some(pkt) = parse_rx_line(&entry.line) {
                    if tx.send(pkt).is_err() {
                        return;
                    }
                } else if let Some(evt) = parse_status_line(&entry.line) {
                    if status_tx.send(evt).is_err() {
                        return;
                    }
                }
            }
            last_seq = new_entries.last().unwrap().seq;
        });

        Ok(Self { base_url, own_addr, events: rx, status_events: status_rx })
    }
}

impl Radio for HttpRadio {
    // Posts to /sendbin (hex-encoded body), not the older /send (raw text
    // body) — a payload here can now be one of the binary hot-path frames
    // (PUNCH/PACK/HB), and lossily reinterpreting arbitrary bytes as UTF-8
    // (the old `String::from_utf8_lossy` approach) would corrupt them.
    // /send's own plain-text contract is untouched — it's still what an
    // operator's manual "send raw payload" action uses — this just gives
    // this daemon-internal caller (e.g. lora-tui forwarding a directly
    // attached SI reader's punches, or its own heartbeat, through a remote
    // daemon) a byte-safe path alongside it. See `to_hex`/`from_hex`.
    fn send(&mut self, _dest: u16, payload: &[u8]) -> Result<()> {
        ureq::post(&format!("{}/sendbin", self.base_url))
            .send_string(&to_hex(payload))
            .map_err(|e| anyhow::anyhow!("POST /sendbin failed: {}", e))?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        Ok(self.events.try_recv().ok())
    }

    fn set_dest(&mut self, dest: u16) -> Result<()> {
        ureq::post(&format!("{}/setdest", self.base_url))
            .send_string(&format!("dest={}", dest))
            .map_err(|e| anyhow::anyhow!("POST /setdest failed: {}", e))?;
        Ok(())
    }

    fn poll_status(&mut self) -> Vec<StatusEvent> {
        self.status_events.try_iter().collect()
    }

    fn send_command(&mut self, _commander: u16, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
        // The daemon fills in its own address as commander when it builds
        // the actual radio frame — an attach client has no radio identity
        // of its own to offer here.
        ureq::post(&format!("{}/cmd", self.base_url))
            .send_string(&format!("target={}&heartbeat_interval_secs={}", target, heartbeat_interval_secs))
            .map_err(|e| anyhow::anyhow!("POST /cmd failed: {}", e))?;
        Ok(())
    }

    fn send_test_punch(&mut self, card_id: u32, station: u8, time_s: u32) -> Result<()> {
        ureq::post(&format!("{}/testpunch", self.base_url))
            .send_string(&format!("card_id={}&station={}&time_s={}", card_id, station, time_s))
            .map_err(|e| anyhow::anyhow!("POST /testpunch failed: {}", e))?;
        Ok(())
    }

    fn clear_punch(&mut self, id: i64) -> Result<()> {
        ureq::post(&format!("{}/clearpunch", self.base_url))
            .send_string(&format!("id={}", id))
            .map_err(|e| anyhow::anyhow!("POST /clearpunch failed: {}", e))?;
        Ok(())
    }

    fn clear_all_punches(&mut self) -> Result<()> {
        ureq::post(&format!("{}/clearpunches", self.base_url))
            .send_string("")
            .map_err(|e| anyhow::anyhow!("POST /clearpunches failed: {}", e))?;
        Ok(())
    }

    fn query_version(&mut self, target: u16) -> Result<()> {
        ureq::post(&format!("{}/queryversion", self.base_url))
            .send_string(&format!("target={}", target))
            .map_err(|e| anyhow::anyhow!("POST /queryversion failed: {}", e))?;
        Ok(())
    }

    fn set_competition_id(&mut self, id: &str) -> Result<()> {
        ureq::post(&format!("{}/setcompetitionid", self.base_url))
            .send_string(&format!("competition_id={}", id))
            .map_err(|e| anyhow::anyhow!("POST /setcompetitionid failed: {}", e))?;
        Ok(())
    }

    fn send_setting(&mut self, _commander: u16, target: u16, setting: Setting) -> Result<()> {
        // Same reasoning as send_command above — the daemon fills in its
        // own address as commander.
        ureq::post(&format!("{}/setcfg", self.base_url))
            .send_string(&format!("target={}&setting={}", target, setting.encode()))
            .map_err(|e| anyhow::anyhow!("POST /setcfg failed: {}", e))?;
        Ok(())
    }
}

/// `addr` is discovered from the attached daemon itself (its own
/// /status.json response), not taken from the caller — lora-tui no longer
/// has its own --addr flag at all. It used to, but that value was purely
/// cosmetic in attach mode (the daemon always fills in its own real address
/// server-side for anything that actually matters — see send_command's doc
/// comment) and had no way to stay in sync with whatever daemon you
/// actually pointed lora-tui at, so a stale/mismatched default was the only
/// possible outcome for anyone not manually overriding it every time.
pub fn attach(server_url: &str, dest: u16) -> Result<()> {
    let server_url = server_url.trim_end_matches('/').to_string();
    let radio = HttpRadio::new(server_url.clone())?;
    let addr = radio.own_addr;
    let (_, si_rx) = std::sync::mpsc::channel();
    crate::ui::run_app(format!("attached via {}", server_url), addr, dest, Box::new(radio), 0, si_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_round_trips_arbitrary_bytes() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)), Some(bytes));
    }

    #[test]
    fn test_hex_encode_matches_known_vector() {
        assert_eq!(to_hex(&[0x00, 0x01, 0xFF]), "0001ff");
        assert_eq!(from_hex("0001ff"), Some(vec![0x00, 0x01, 0xFF]));
    }

    #[test]
    fn test_from_hex_rejects_odd_length_or_non_hex() {
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex(""), Some(vec![]));
    }

    #[test]
    fn test_next_local_batch_empty_buffer_is_none() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        assert!(next_local_batch(&buffer, 5, 1).unwrap().is_none());
    }

    #[test]
    fn test_next_local_batch_groups_same_card_id_and_matches_wire_format() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        let id1 = buffer.record(123456, 31, 36070, "local").unwrap();
        let id2 = buffer.record(123456, 32, 36200, "local").unwrap();

        let (card_id, row_ids, payload) = next_local_batch(&buffer, 5, 1).unwrap().unwrap();
        assert_eq!(card_id, 123456);
        assert_eq!(row_ids, vec![id1, id2]);

        let expected = crate::sportident::CardReadout {
            card_id: 123456,
            punches: vec![
                crate::sportident::ControlPunch { station: 31, time_s: 36070 },
                crate::sportident::ControlPunch { station: 32, time_s: 36200 },
            ],
        }.to_payload(5, 1);
        assert_eq!(payload, expected);
    }

    #[test]
    fn test_next_local_batch_does_not_mix_different_card_ids() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        buffer.record(1, 1, 100, "local").unwrap();
        buffer.record(2, 2, 200, "local").unwrap();

        let (card_id, row_ids, _) = next_local_batch(&buffer, 5, 1).unwrap().unwrap();
        assert_eq!(card_id, 1);
        assert_eq!(row_ids.len(), 1);
    }

    #[test]
    fn test_next_local_batch_ignores_remote_sourced_rows() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        buffer.record(1, 1, 100, "192.168.1.5").unwrap();
        let id = buffer.record(2, 2, 200, "local").unwrap();

        let (card_id, row_ids, _) = next_local_batch(&buffer, 5, 1).unwrap().unwrap();
        assert_eq!(card_id, 2);
        assert_eq!(row_ids, vec![id]);
    }

    #[test]
    fn test_parse_status_line_cmdok() {
        let evt = parse_status_line("CMDOK 5 hb_interval=30").unwrap();
        match evt {
            StatusEvent::CmdOk { target, setting } => {
                assert_eq!(target, 5);
                assert_eq!(setting, Setting::HeartbeatIntervalSecs(30));
            }
            _ => panic!("expected CmdOk"),
        }
    }

    #[test]
    fn test_parse_status_line_cmderr() {
        let evt = parse_status_line("CMDERR 7 hb_interval=45").unwrap();
        match evt {
            StatusEvent::CmdErr { target, setting } => {
                assert_eq!(target, 7);
                assert_eq!(setting, Setting::HeartbeatIntervalSecs(45));
            }
            _ => panic!("expected CmdErr"),
        }
    }

    #[test]
    fn test_parse_status_line_still_handles_existing_events() {
        assert!(matches!(parse_status_line("HB 3").unwrap(), StatusEvent::Heartbeat { dest: 3 }));
        assert!(matches!(parse_status_line("ERR boom").unwrap(), StatusEvent::Err(m) if m == "boom"));
        assert!(parse_status_line("garbage").is_none());
    }

    #[test]
    fn test_parse_status_line_hbrx_with_battery_and_si() {
        let evt = parse_status_line("HBRX 5 82 3950 1").unwrap();
        assert_eq!(evt, StatusEvent::HeartbeatRx { node: 5, battery: Some((82, 3950)), si_present: Some(true) });
    }

    #[test]
    fn test_parse_status_line_hbrx_no_battery_si_absent() {
        let evt = parse_status_line("HBRX 5 - 0").unwrap();
        assert_eq!(evt, StatusEvent::HeartbeatRx { node: 5, battery: None, si_present: Some(false) });
    }

    #[test]
    fn test_parse_status_line_hbrx_no_si_reported() {
        // Old-style relay/no-SI-concept heartbeat forward: "-" for si means
        // "not reported", distinct from an explicit "0" (SI master absent).
        let evt = parse_status_line("HBRX 5 - -").unwrap();
        assert_eq!(evt, StatusEvent::HeartbeatRx { node: 5, battery: None, si_present: None });
    }

    #[test]
    fn test_parse_status_line_packok() {
        let evt = parse_status_line("PACKOK 123456 5").unwrap();
        assert_eq!(evt, StatusEvent::PunchAckOk { card_id: 123456, acked_by: 5 });
    }

    #[test]
    fn test_parse_status_line_cmdapplied() {
        let evt = parse_status_line("CMDAPPLIED 1 hb_interval=30").unwrap();
        assert_eq!(evt, StatusEvent::CmdApplied { commander: 1, setting: Setting::HeartbeatIntervalSecs(30) });
    }

    #[test]
    fn test_parse_status_line_punchrx() {
        let evt = parse_status_line("PUNCHRX 10 123456").unwrap();
        assert_eq!(evt, StatusEvent::PunchRx { origin: 10, card_id: 123456 });
    }

    #[test]
    fn test_parse_status_line_versionrx() {
        let evt = parse_status_line("VERSIONRX 10 0.1.0+a1b2c3d4.dirty").unwrap();
        assert_eq!(evt, StatusEvent::VersionRx { origin: 10, version: "0.1.0+a1b2c3d4.dirty".to_string() });
    }

    fn free_test_listen_addr() -> String {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("127.0.0.1:{port}")
    }

    fn wait_for_server(base_url: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if ureq::get(&format!("{base_url}/status.json")).call().is_ok() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("server at {base_url} never came up");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Real end-to-end check of HttpRadio's command path against a real
    /// web::spawn_server instance: send_command should reach the daemon's
    /// command channel as exactly "CMD <target> <secs>" — what
    /// run_daemon_loop's cmd_rx dispatch expects.
    #[test]
    fn test_http_radio_send_command_reaches_daemon_command_channel() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let listen = free_test_listen_addr();
        crate::web::spawn_server(listen.clone(), 5, state, radio_ready, None, cmd_tx, Arc::new(Mutex::new("evt-1".to_string())));
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        let mut radio = HttpRadio::new(base_url).unwrap();
        // Discovered from /status.json's own_addr (this server was spawned
        // with own_addr=5 above), not from any caller-supplied value — this
        // is what attach() uses in place of a --addr flag now.
        assert_eq!(radio.own_addr, 5);
        radio.send_command(1, 5, 30).unwrap();

        let cmd = cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(cmd, "CMD 5 30");
    }

    /// Confirms the other half of HttpRadio: log lines pushed into
    /// daemon_state (as run_daemon_loop's log_event would) actually surface
    /// as a ReceivedPacket via receive() and a StatusEvent via poll_status()
    /// after the background poller picks them up from /status.json — not
    /// just that HttpRadio compiles against the trait.
    #[test]
    fn test_http_radio_polls_new_log_lines_into_events_and_status() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<String>();
        let listen = free_test_listen_addr();
        crate::web::spawn_server(listen.clone(), 5, Arc::clone(&state), radio_ready, None, cmd_tx, Arc::new(Mutex::new("evt-1".to_string())));
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        let mut radio = HttpRadio::new(base_url).unwrap();

        state.lock().unwrap().push_log("RX 5 -80 PUNCH 5 123456 31:100".to_string());
        state.lock().unwrap().push_log("HBRX 5 77 3900 1".to_string());

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got_pkt = false;
        let mut got_evt = false;
        while Instant::now() < deadline && !(got_pkt && got_evt) {
            if let Ok(Some(pkt)) = radio.receive() {
                assert_eq!(pkt.src_addr, 5);
                got_pkt = true;
            }
            for evt in radio.poll_status() {
                if let StatusEvent::HeartbeatRx { node, battery, si_present } = evt {
                    assert_eq!(node, 5);
                    assert_eq!(battery, Some((77, 3900)));
                    assert_eq!(si_present, Some(true));
                    got_evt = true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(got_pkt, "never received the RX-derived packet via HTTP polling");
        assert!(got_evt, "never received the HBRX-derived status event via HTTP polling");
    }

    #[test]
    fn test_heartbeat_round_trips_bare() {
        let encoded = encode_heartbeat(None, None);
        assert_eq!(encoded, vec![TAG_HB, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(parse_heartbeat(&encoded), Some(Heartbeat { battery: None, si_present: None }));
    }

    #[test]
    fn test_heartbeat_round_trips_with_battery_and_si_present() {
        let encoded = encode_heartbeat(Some((82, 3950)), Some(true));
        assert_eq!(
            parse_heartbeat(&encoded),
            Some(Heartbeat { battery: Some((82, 3950)), si_present: Some(true) })
        );
        let encoded = encode_heartbeat(Some((82, 3950)), Some(false));
        assert_eq!(
            parse_heartbeat(&encoded),
            Some(Heartbeat { battery: Some((82, 3950)), si_present: Some(false) })
        );
    }

    #[test]
    fn test_heartbeat_round_trips_no_battery_but_si_present() {
        let encoded = encode_heartbeat(None, Some(true));
        assert_eq!(parse_heartbeat(&encoded), Some(Heartbeat { battery: None, si_present: Some(true) }));
    }

    #[test]
    fn test_parse_heartbeat_rejects_wrong_tag_or_length() {
        let mut wrong_tag = encode_heartbeat(Some((82, 3950)), Some(true));
        wrong_tag[0] = 0xFF;
        assert_eq!(parse_heartbeat(&wrong_tag), None);
        assert_eq!(parse_heartbeat(b""), None);
        assert_eq!(parse_heartbeat(&encode_heartbeat(None, None)[..4]), None);
    }

    /// A radio double that just records what was sent and lets a test queue
    /// up canned received packets — no real hardware needed.
    struct FakeRadio {
        sent: Vec<(u16, Vec<u8>)>,
        to_receive: std::collections::VecDeque<ReceivedPacket>,
    }

    impl Radio for FakeRadio {
        fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
            self.sent.push((dest, payload.to_vec()));
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
            Ok(self.to_receive.pop_front())
        }
    }

    // Generic over `impl AsRef<[u8]>` so the same helper builds a
    // ReceivedPacket from either a text frame (`&str`) or one of the binary
    // hot-path frames (`&Vec<u8>`/`&[u8]`) without needing two near-identical
    // functions.
    fn packet(src_addr: u16, payload: impl AsRef<[u8]>) -> ReceivedPacket {
        let mut p = heapless::Vec::<u8, 240>::new();
        let _ = p.extend_from_slice(payload.as_ref());
        ReceivedPacket { src_addr, payload: p, rssi: None }
    }

    /// Real run_daemon_loop, not just PunchBuffer::clear_local_unsent in
    /// isolation: proves CLEARPUNCH also cancels an *in-flight* pending_punch,
    /// not just the DB row. Without that cancellation the loop would keep
    /// retrying the now-deleted payload forever (pending_punch is only
    /// re-derived from the buffer once it's None), so the second buffered
    /// punch would never go out — the assertion below is exactly the
    /// regression that cancellation prevents.
    #[test]
    fn test_clearpunch_cancels_in_flight_pending_punch_so_next_punch_can_go_out() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let id1 = punch_buffer.record(111, 31, 100, "local").unwrap();

        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();

        let pb = Arc::clone(&punch_buffer);
        let state_for_loop = Arc::clone(&state);
        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 5, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, pb, state_for_loop,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !state.lock().unwrap().log.iter().any(|e| e.line.contains("111")) {
            assert!(Instant::now() < deadline, "first punch (card 111) was never sent");
            std::thread::sleep(Duration::from_millis(10));
        }

        // Only after the first punch is truly in flight — buffer a second,
        // different-card punch now, so it's not part of the first batch.
        punch_buffer.record(222, 32, 200, "local").unwrap();
        cmd_tx.send(format!("CLEARPUNCH {}", id1)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !state.lock().unwrap().log.iter().any(|e| e.line.contains("222")) {
            assert!(Instant::now() < deadline, "clearing the in-flight punch never let the next one go out");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(punch_buffer.unsent().unwrap().iter().all(|p| p.id != id1));
    }

    #[test]
    fn test_clearpunches_command_removes_all_unsent_punches() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        punch_buffer.record(111, 31, 100, "local").unwrap();
        punch_buffer.record(222, 32, 200, "local").unwrap();

        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();

        let pb = Arc::clone(&punch_buffer);
        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 5, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, pb, state,
            );
        });

        cmd_tx.send("CLEARPUNCHES".to_string()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !punch_buffer.unsent().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "CLEARPUNCHES never cleared the buffer");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Real run_daemon_loop: a node whose own dest happens to equal its own
    /// address (easy to end up with by accident — see the skip's own doc
    /// comment at the call site) must not send itself a heartbeat. Uses a
    /// short real interval and waits past it, then confirms no "HB " line
    /// ever appears in the daemon log.
    #[test]
    fn test_heartbeat_skipped_when_dest_equals_own_addr() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 1, dest: 1, heartbeat_interval: 1, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        std::thread::sleep(Duration::from_millis(1200)); // past the 1s heartbeat interval
        let has_hb_line = state_for_check.lock().unwrap().log.iter().any(|e| e.line.starts_with("HB "));
        assert!(!has_hb_line, "sent a heartbeat to itself: dest == own_addr");

        drop(cmd_tx);
    }

    /// Same bug, punch-sending side: reported live as "the web interface's
    /// test-punch button is annoying" — a base station whose dest equals
    /// its own address kept radio-retrying a self-addressed punch forever.
    /// A buffered local/test punch should be left alone (still unsent, so
    /// the HTTP pusher can still pick it up if --push-to is configured)
    /// rather than radio-transmitted or retried toward itself.
    #[test]
    fn test_local_punch_send_skipped_when_dest_equals_own_addr() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        punch_buffer.record(111, 31, 100, "test").unwrap();

        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);
        let pb = Arc::clone(&punch_buffer);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 1, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, pb, state,
            );
        });

        std::thread::sleep(Duration::from_millis(300));
        let attempted_send = state_for_check
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|e| e.line.starts_with("TX ") || e.line.starts_with("ERR TX"));
        assert!(!attempted_send, "attempted to radio-send a local punch to itself: dest == own_addr");
        assert_eq!(
            punch_buffer.unsent().unwrap().len(), 1,
            "punch should remain buffered, not consumed by a self-send attempt"
        );

        drop(cmd_tx);
    }

    /// Real run_daemon_loop, not just CardReadout::parse_payload in
    /// isolation: LoRa is a broadcast medium (see to_payload's doc comment
    /// in sportident.rs) — every node in range decodes every packet
    /// regardless of what `dest` its sender used, so the embedded `dest`
    /// field is the only thing that stops a non-relay node from also
    /// consuming/acking traffic addressed to someone else. Feeds two
    /// overheard punches through a real (non-relay) daemon loop — one
    /// addressed to it, one not — and confirms only the addressed one ends
    /// up buffered.
    #[test]
    fn test_run_daemon_loop_only_consumes_punches_addressed_to_own_addr() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());

        let ours = crate::sportident::CardReadout {
            card_id: 111,
            punches: vec![crate::sportident::ControlPunch { station: 1, time_s: 100 }],
        }.to_payload(10, 2); // origin 10, dest 2 — matches own_addr below
        let not_ours = crate::sportident::CardReadout {
            card_id: 222,
            punches: vec![crate::sportident::ControlPunch { station: 1, time_s: 200 }],
        }.to_payload(10, 99); // dest 99 — a different node entirely

        let radio: Box<dyn Radio> = Box::new(FakeRadio {
            sent: Vec::new(),
            to_receive: std::collections::VecDeque::from(vec![packet(10, &ours), packet(10, &not_ours)]),
        });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        let pb = Arc::clone(&punch_buffer);
        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, pb, state,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let unsent = punch_buffer.unsent().unwrap();
            if unsent.iter().any(|p| p.card_id == 111) {
                assert!(unsent.iter().all(|p| p.card_id != 222), "punch not addressed to us was consumed anyway");
                break;
            }
            assert!(Instant::now() < deadline, "the punch addressed to us was never buffered");
            std::thread::sleep(Duration::from_millis(10));
        }

        // The consumed punch (only the one addressed to us) should also
        // have bumped node 10's punch_count via the real record_punch call
        // in run_daemon_loop, not just via calling it in isolation.
        assert_eq!(state_for_check.lock().unwrap().nodes[&10].punch_count, 1);

        drop(cmd_tx);
    }

    /// Real run_daemon_loop, not the pure protocol::Frame round-trip already
    /// covered in protocol.rs: confirms a VersionQuery addressed to us
    /// actually gets a VersionReport carrying this daemon's own real
    /// VERSION sent back, over a real (fake) radio.
    #[test]
    fn test_run_daemon_loop_replies_to_version_query_addressed_to_own_addr() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let query = Frame::VersionQuery { target: 2 }.encode();

        let radio: Box<dyn Radio> = Box::new(FakeRadio {
            sent: Vec::new(),
            to_receive: std::collections::VecDeque::from(vec![packet(10, &query)]),
        });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        let expected_reply = Frame::VersionReport { origin: 2, version: crate::version::VERSION.to_string() }.encode();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let saw_reply = state_for_check
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|e| e.line == format!("TX 10 {}", expected_reply));
            if saw_reply {
                break;
            }
            assert!(Instant::now() < deadline, "never replied to the version query addressed to us");
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(cmd_tx);
    }

    /// A VersionReport is recorded from whoever sent it, unconditionally —
    /// same accepted no-addressing limitation as HB — so this daemon learns
    /// a node's version passively even when it never asked for it.
    #[test]
    fn test_run_daemon_loop_records_version_report_from_any_node() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let report = Frame::VersionReport { origin: 10, version: "0.1.0+a1b2c3d4".to_string() }.encode();

        let radio: Box<dyn Radio> = Box::new(FakeRadio {
            sent: Vec::new(),
            to_receive: std::collections::VecDeque::from(vec![packet(10, &report)]),
        });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if state_for_check.lock().unwrap().nodes.get(&10).and_then(|n| n.version.clone()).is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "version report from node 10 was never recorded");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(state_for_check.lock().unwrap().nodes[&10].version.as_deref(), Some("0.1.0+a1b2c3d4"));

        drop(cmd_tx);
    }

    /// Receiving a VersionReport — whether it's a reply to our own VQUERY
    /// or a node's unprompted boot announcement — always earns that node a
    /// ConfigAck back, since it's the only signal a freshly booted node has
    /// that its current LoRa mode is actually reaching this base station
    /// (see esp32-node's boot-verification logic and Frame::ConfigAck's
    /// doc comment).
    #[test]
    fn test_run_daemon_loop_sends_config_ack_for_any_version_report() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let report = Frame::VersionReport { origin: 10, version: "0.1.0+a1b2c3d4".to_string() }.encode();

        let radio: Box<dyn Radio> = Box::new(FakeRadio {
            sent: Vec::new(),
            to_receive: std::collections::VecDeque::from(vec![packet(10, &report)]),
        });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        let expected_tx = format!("TX 10 {}", Frame::ConfigAck { target: 10 }.encode());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let sent = state_for_check.lock().unwrap().log.iter().any(|e| e.line == expected_tx);
            if sent {
                break;
            }
            assert!(Instant::now() < deadline, "config ack for node 10 was never sent");
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(cmd_tx);
    }

    /// A QUERYVERSION command (as originated by POST /queryversion — see
    /// web.rs, or lora-tui's /queryversion) sends a real VersionQuery frame
    /// out over the radio toward the daemon's configured `dest`.
    #[test]
    fn test_run_daemon_loop_sends_version_query_on_command() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        cmd_tx.send("QUERYVERSION 10".to_string()).unwrap();

        let expected_tx = format!("TX 1 {}", Frame::VersionQuery { target: 10 }.encode());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let sent = state_for_check.lock().unwrap().log.iter().any(|e| e.line == expected_tx);
            if sent {
                break;
            }
            assert!(Instant::now() < deadline, "QUERYVERSION command never produced a VQUERY send");
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(cmd_tx);
    }

    /// SETCFG is the generic counterpart to CMD (see protocol.rs's module
    /// doc comment) — proves it reaches a real run_daemon_loop and produces
    /// a Command frame carrying whatever Setting it was given, not just
    /// HeartbeatIntervalSecs.
    #[test]
    fn test_run_daemon_loop_sends_command_frame_on_setcfg() {
        let punch_buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(":memory:").unwrap());
        let radio: Box<dyn Radio> = Box::new(FakeRadio { sent: Vec::new(), to_receive: Default::default() });
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
        let (_si_tx, si_rx) = std::sync::mpsc::channel();
        let state = crate::daemon_state::new_shared();
        let state_for_check = Arc::clone(&state);

        std::thread::spawn(move || {
            let _ = run_daemon_loop(
                DaemonIdentity { own_addr: 2, dest: 1, heartbeat_interval: 0, relay: false },
                cmd_rx, radio, si_rx, punch_buffer, state,
            );
        });

        cmd_tx.send("SETCFG 10 sf=11".to_string()).unwrap();

        let expected_frame = Frame::Command { target: 10, commander: 2, setting: Setting::Sf(11) };
        let expected_tx = format!("TX 1 {}", expected_frame.encode());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let sent = state_for_check.lock().unwrap().log.iter().any(|e| e.line == expected_tx);
            if sent {
                break;
            }
            assert!(Instant::now() < deadline, "SETCFG command never produced a Command send");
            std::thread::sleep(Duration::from_millis(10));
        }

        drop(cmd_tx);
    }
}
