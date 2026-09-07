use anyhow::Result;
use sx127x::ReceivedPacket;
use std::io::{BufRead, BufReader, BufWriter, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{Frame, Setting};
use crate::Args;

// ── Delay ─────────────────────────────────────────────────────────────────────

struct StdDelay;

impl embedded_hal::delay::DelayNs for StdDelay {
    fn delay_ns(&mut self, ns: u32) {
        std::thread::sleep(std::time::Duration::from_nanos(ns as u64));
    }
}

// ── Radio trait ───────────────────────────────────────────────────────────────

/// Out-of-band activity the daemon broadcasts to attached clients that isn't
/// an incoming radio packet (its own outgoing heartbeats/sends, or errors).
/// Real radio backends have nothing to report here.
pub enum StatusEvent {
    Heartbeat { dest: u16 },
    Tx { dest: u16, payload: String },
    Err(String),
    /// A command this daemon originated was confirmed applied by its target.
    CmdOk { target: u16, setting: Setting },
    /// A command this daemon originated got no ack after retrying.
    CmdErr { target: u16, setting: Setting },
}

pub trait Radio: Send {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<ReceivedPacket>>;
    fn set_dest(&mut self, _dest: u16) -> Result<()> { Ok(()) }
    fn poll_status(&mut self) -> Vec<StatusEvent> { Vec::new() }

    /// Ask the node at `target` to change its heartbeat interval, as
    /// `commander` (this node's own address). Direct hardware backends send
    /// this as a single, untracked frame — a human watching the TUI can
    /// retry manually if no ack shows up. The daemon's SocketRadio instead
    /// hands this off to the daemon itself, which owns retry-with-ack
    /// tracking (see run_daemon_loop).
    fn send_command(&mut self, commander: u16, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
        let frame = Frame::Command { target, commander, setting: Setting::HeartbeatIntervalSecs(heartbeat_interval_secs) };
        self.send(target, frame.encode().as_bytes())
    }
}

// ── Network identification ──────────────────────────────────────────────────

/// Wraps any `Radio` to transparently prepend/strip a shared deployment
/// network ID on every send/receive (see docs/protocols/
/// lora_online_control_protocol.md, "Network Identification"). Guards
/// against accidental cross-talk from another deployment of this same
/// open-source firmware nearby, or unrelated gear that happens to share our
/// sync word — a plain-text prefix, not cryptographic, since the actual
/// threat model here is accidental collision between independent events,
/// not a deliberate spoofer with access to the source. `Frame::parse`/
/// `CardReadout::parse_payload` stay completely unaware this exists — they
/// only ever see a payload after the network ID has already been stripped.
struct NetworkFilteredRadio<R: Radio> {
    inner: R,
    network_id: String,
}

impl<R: Radio> NetworkFilteredRadio<R> {
    fn new(inner: R, network_id: String) -> Self {
        Self { inner, network_id }
    }
}

impl<R: Radio> Radio for NetworkFilteredRadio<R> {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
        let mut framed = Vec::with_capacity(self.network_id.len() + 1 + payload.len());
        framed.extend_from_slice(self.network_id.as_bytes());
        framed.push(b' ');
        framed.extend_from_slice(payload);
        self.inner.send(dest, &framed)
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        let Some(pkt) = self.inner.receive()? else { return Ok(None) };
        let Ok(text) = core::str::from_utf8(&pkt.payload) else { return Ok(None) };
        let Some(rest) = text.strip_prefix(self.network_id.as_str()).and_then(|s| s.strip_prefix(' ')) else {
            // Not ours — a different deployment, or unrelated traffic that
            // happens to share our radio settings. Treat as if nothing was
            // received rather than misparsing someone else's frame.
            return Ok(None);
        };
        let mut payload = heapless::Vec::<u8, 240>::new();
        let _ = payload.extend_from_slice(rest.as_bytes());
        Ok(Some(ReceivedPacket { src_addr: pkt.src_addr, payload, rssi: pkt.rssi }))
    }

    fn set_dest(&mut self, dest: u16) -> Result<()> {
        self.inner.set_dest(dest)
    }

    fn poll_status(&mut self) -> Vec<StatusEvent> {
        self.inner.poll_status()
    }
}

// ── Config builder ────────────────────────────────────────────────────────────

fn build_sx127x_config(args: &Args) -> Result<sx127x::Config> {
    use sx127x::{Bandwidth, CodingRate};

    let bandwidth = match args.bw_hz {
        7_800 => Bandwidth::Khz7_8,
        10_400 => Bandwidth::Khz10_4,
        15_600 => Bandwidth::Khz15_6,
        20_800 => Bandwidth::Khz20_8,
        31_250 => Bandwidth::Khz31_25,
        41_700 => Bandwidth::Khz41_7,
        62_500 => Bandwidth::Khz62_5,
        125_000 => Bandwidth::Khz125,
        250_000 => Bandwidth::Khz250,
        500_000 => Bandwidth::Khz500,
        b => anyhow::bail!("unsupported bandwidth {}Hz", b),
    };
    let coding_rate = match args.cr {
        5 => CodingRate::Cr4_5,
        6 => CodingRate::Cr4_6,
        7 => CodingRate::Cr4_7,
        8 => CodingRate::Cr4_8,
        c => anyhow::bail!("unsupported coding rate 4/{} — use 5, 6, 7, or 8", c),
    };
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
            Ok(Box::new(NetworkFilteredRadio::new(driver, args.network_id.clone())))
        } else {
            let mut driver = sx127x::Sx127xSpi::new(spi_device, reset, StdDelay);
            sx127x::LoraRadio::configure(&mut driver, &config).map_err(|e| anyhow::anyhow!("{}", e))?;
            Ok(Box::new(NetworkFilteredRadio::new(driver, args.network_id.clone())))
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

    let (clients, cmd_rx, cmd_tx_for_web) = setup_daemon_socket(&args.socket)?;
    let state = crate::daemon_state::new_shared();
    crate::web::spawn_server(
        args.web_listen.clone(), Arc::clone(&state), Arc::clone(&radio_ready), args.roc_health_url.clone(), cmd_tx_for_web,
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

    let punch_buffer = setup_punch_pipeline(&args)?;
    run_daemon_loop(
        DaemonIdentity { own_addr: args.addr, dest: args.dest, heartbeat_interval: args.heartbeat_interval, relay: args.relay },
        clients, cmd_rx, radio, si_rx, punch_buffer, state,
    )
}

// ── Daemon socket + loop ──────────────────────────────────────────────────────

type Clients = Arc<Mutex<Vec<std::sync::mpsc::SyncSender<String>>>>;

fn broadcast(clients: &Clients, msg: String) {
    let mut guard = clients.lock().unwrap();
    guard.retain(|tx| tx.try_send(msg.clone()).is_ok());
}

/// Every place that already calls `broadcast` for lora-tui's live stream
/// also wants this same line in the packet log the web dashboard reads —
/// one call covers both instead of duplicating a push_log at every site.
fn log_and_broadcast(clients: &Clients, state: &crate::daemon_state::SharedState, msg: String) {
    state.lock().unwrap().push_log(msg.clone());
    broadcast(clients, msg);
}

fn setup_daemon_socket(
    socket_path: &str,
) -> Result<(Clients, std::sync::mpsc::Receiver<String>, std::sync::mpsc::Sender<String>)> {
    use std::os::unix::net::UnixListener;
    use std::os::unix::fs::PermissionsExt;

    let _ = std::fs::remove_file(socket_path);
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))?;

    let clients: Clients = Arc::new(Mutex::new(Vec::new()));
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<String>();
    // Kept for the web server (web.rs) to inject TESTPUNCH through the same
    // command channel real socket clients use — one parsing/validation path
    // for the command regardless of which interface triggered it.
    let cmd_tx_for_web = cmd_tx.clone();

    let listener_clients = Arc::clone(&clients);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let c = Arc::clone(&listener_clients);
            let tx = cmd_tx.clone();
            std::thread::spawn(move || handle_client(stream, c, tx));
        }
    });

    log::info!("socket ready at {}", socket_path);
    Ok((clients, cmd_rx, cmd_tx_for_web))
}

fn handle_client(
    stream: std::os::unix::net::UnixStream,
    clients: Clients,
    cmd_tx: std::sync::mpsc::Sender<String>,
) {
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(100);
    clients.lock().unwrap().push(event_tx);

    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    std::thread::spawn(move || {
        let reader = BufReader::new(read_stream);
        for line in reader.lines().flatten() {
            if line.starts_with("SEND ") || line.starts_with("SET_DEST ") || line.starts_with("CMD ")
                || line.starts_with("TESTPUNCH ") {
                let _ = cmd_tx.send(line);
            }
        }
    });

    let mut writer = BufWriter::new(stream);
    for event in event_rx {
        if writeln!(writer, "{}", event).is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// Opens the persistent punch buffer and, if `--push-to` is configured,
/// starts the background pusher thread. Called once per daemon startup;
/// the returned buffer is fed by run_daemon_loop for both local and remote
/// punches regardless of whether pushing is enabled.
fn setup_punch_pipeline(args: &Args) -> Result<Arc<crate::punch_buffer::PunchBuffer>> {
    if let Some(parent) = std::path::Path::new(&args.punch_db).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let buffer = Arc::new(crate::punch_buffer::PunchBuffer::open(&args.punch_db)?);

    if let Some(push_to) = &args.push_to {
        crate::pusher::spawn(
            Arc::clone(&buffer),
            push_to.clone(),
            Duration::from_secs(args.push_interval_secs),
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
    payload: String,
    sent_at: Instant,
    attempts: u32,
}

/// Parses a heartbeat payload: bare `HB` (no battery — mains-powered relays,
/// or a device without battery sensing wired) or `HB <pct> <mv>` (battery
/// percent 0-100, millivolts — see esp32-node/src/battery.rs). Returns
/// `None` if `payload` isn't a heartbeat at all; `Some(None)`/`Some(Some(..))`
/// distinguish "is a heartbeat, no battery data" from "has battery data".
fn parse_heartbeat(payload: &str) -> Option<Option<(u8, u16)>> {
    if payload == "HB" {
        return Some(None);
    }
    let rest = payload.strip_prefix("HB ")?;
    let mut parts = rest.splitn(2, ' ');
    let pct: u8 = parts.next()?.parse().ok()?;
    let mv: u16 = parts.next()?.parse().ok()?;
    Some(Some((pct, mv)))
}

/// The next batch of local, unsent punches to (re)transmit: the oldest
/// unsent local row, plus any immediately-following unsent local rows that
/// share its card_id (reconstructing the original CardReadout's grouping,
/// since punches from one card tap are buffered as consecutive rows sharing
/// one card_id). Returns the row ids covered (to mark sent once acked) and
/// the exact payload to send, rebuilt via `CardReadout::to_payload()` so it
/// matches the wire format precisely.
fn next_local_batch(buffer: &crate::punch_buffer::PunchBuffer, own_addr: u16) -> Result<Option<(u32, Vec<i64>, String)>> {
    let unsent = buffer.unsent_local()?;
    let Some(first) = unsent.first() else { return Ok(None) };
    let card_id = first.card_id;
    let batch: Vec<_> = unsent.iter().take_while(|p| p.card_id == card_id).collect();
    let row_ids = batch.iter().map(|p| p.id).collect();
    let punches = batch.iter()
        .map(|p| crate::sportident::ControlPunch { station: p.station, time_s: p.time_s })
        .collect();
    let payload = crate::sportident::CardReadout { card_id, punches }.to_payload(own_addr);
    Ok(Some((card_id, row_ids, payload)))
}

/// Sends a Command frame to the radio-layer address `dest` — this node's
/// own configured next hop, not necessarily `target` directly. That's what
/// makes relaying possible: if `target` isn't in direct range, `dest` is
/// the relay that is, exactly mirroring how uplink punch traffic already
/// routes via each node's own `--dest` rather than straight to the final
/// destination. Commanding a node other than the current `--dest` now needs
/// a `SET_DEST` first, same as the existing `SEND` socket command already
/// requires — this makes `CMD` consistent with the rest of the protocol
/// instead of being the one exception that assumed direct reach.
fn send_command_frame(
    radio: &mut dyn Radio, clients: &Clients, state: &crate::daemon_state::SharedState,
    dest: u16, target: u16, commander: u16, setting: Setting,
) {
    let frame = Frame::Command { target, commander, setting };
    let payload = frame.encode();
    match radio.send(dest, payload.as_bytes()) {
        Ok(()) => {
            log::info!("CMD to {} (via {}): {}", target, dest, payload);
            log_and_broadcast(clients, state, format!("TX {} {}", dest, payload));
        }
        Err(e) => {
            log::error!("CMD send failed: {}", e);
            log_and_broadcast(clients, state, format!("ERR CMD: {}", e));
        }
    }
}

/// Re-transmits `raw_payload` unchanged toward `next_hop` — a relay's only
/// job for traffic that isn't its own: pass it on, not track or retry it at
/// this hop. Overall reliability still comes from the end-to-end stop-and-
/// wait between the original sender and the final consumer; a lost forward
/// just means that sender's own retry resends the punch, which gets
/// forwarded again.
fn forward(
    radio: &mut dyn Radio, clients: &Clients, state: &crate::daemon_state::SharedState,
    next_hop: u16, raw_payload: &str,
) {
    match radio.send(next_hop, raw_payload.as_bytes()) {
        Ok(()) => {
            log::info!("relayed to {}: {}", next_hop, raw_payload);
            log_and_broadcast(clients, state, format!("TX {} {}", next_hop, raw_payload));
        }
        Err(e) => {
            log::error!("relay forward failed: {}", e);
            log_and_broadcast(clients, state, format!("ERR TX: {}", e));
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
    clients: Clients,
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
                        log_and_broadcast(&clients, &state, format!("TX {} {}", dest, payload));
                    }
                    Err(e) => {
                        log::error!("TX failed: {}", e);
                        log_and_broadcast(&clients, &state, format!("ERR TX: {}", e));
                    }
                }
            } else if let Some(rest) = cmd.strip_prefix("CMD ") {
                let mut parts = rest.splitn(2, ' ');
                if let (Some(target_str), Some(secs_str)) = (parts.next(), parts.next()) {
                    if let (Ok(target), Ok(secs)) = (target_str.parse::<u16>(), secs_str.parse::<u32>()) {
                        let setting = Setting::HeartbeatIntervalSecs(secs);
                        send_command_frame(radio.as_mut(), &clients, &state, dest, target, own_addr, setting);
                        pending_commands.push(PendingCommand { target, setting, sent_at: Instant::now(), attempts: 1 });
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
                                        log_and_broadcast(&clients, &state, format!("TESTPUNCHOK {} {} {}", card_id, station, time_s));
                                    }
                                    Err(e) => {
                                        log::error!("failed to record test punch: {}", e);
                                        log_and_broadcast(&clients, &state, format!("ERR TESTPUNCH: {}", e));
                                    }
                                }
                            }
                            _ => log_and_broadcast(&clients, &state, "ERR TESTPUNCH: bad numeric fields".to_string()),
                        }
                    }
                    _ => log_and_broadcast(&clients, &state, "ERR TESTPUNCH: usage TESTPUNCH <card_id> <station> <time_s>".to_string()),
                }
            }
        }

        pending_commands.retain_mut(|cmd| {
            if cmd.sent_at.elapsed() < CMD_RETRY_INTERVAL {
                return true;
            }
            if cmd.attempts >= CMD_MAX_ATTEMPTS {
                log::warn!("CMD to {} ({:?}) gave up after {} attempts", cmd.target, cmd.setting, cmd.attempts);
                log_and_broadcast(&clients, &state, format!("CMDERR {} {}", cmd.target, cmd.setting.encode()));
                return false;
            }
            cmd.attempts += 1;
            cmd.sent_at = Instant::now();
            send_command_frame(radio.as_mut(), &clients, &state, dest, cmd.target, own_addr, cmd.setting);
            true
        });

        if let Some(period) = heartbeat_period {
            if last_heartbeat.elapsed() >= period {
                last_heartbeat = Instant::now();
                match radio.send(dest, b"HB") {
                    Ok(()) => {
                        log::info!("HB sent to {}", dest);
                        log_and_broadcast(&clients, &state, format!("HB {}", dest));
                    }
                    Err(e) => {
                        log::error!("HB send failed: {}", e);
                        log_and_broadcast(&clients, &state, format!("ERR HB: {}", e));
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

        if pending_punch.is_none() {
            match next_local_batch(&punch_buffer, own_addr) {
                Ok(Some((card_id, row_ids, payload))) => {
                    match radio.send(dest, payload.as_bytes()) {
                        Ok(()) => {
                            log::info!("PUNCH to {}: {}", dest, payload);
                            log_and_broadcast(&clients, &state, format!("TX {} {}", dest, payload));
                            pending_punch = Some(PendingPunch { card_id, row_ids, payload, sent_at: Instant::now(), attempts: 1 });
                        }
                        Err(e) => {
                            log::error!("PUNCH send failed: {}", e);
                            log_and_broadcast(&clients, &state, format!("ERR TX: {}", e));
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
                match radio.send(dest, p.payload.as_bytes()) {
                    Ok(()) => {
                        log::info!("PUNCH retry #{} to {}: {}", p.attempts, dest, p.payload);
                        log_and_broadcast(&clients, &state, format!("TX {} {}", dest, p.payload));
                    }
                    Err(e) => {
                        log::error!("PUNCH retry failed: {}", e);
                        log_and_broadcast(&clients, &state, format!("ERR TX: {}", e));
                    }
                }
            }
        }

        match radio.receive() {
            Ok(Some(pkt)) => {
                let payload = String::from_utf8_lossy(&pkt.payload).into_owned();
                let rssi_str = pkt.rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".to_string());
                match pkt.rssi {
                    Some(dbm) => log::info!("RX from {}: {} (RSSI: {}dBm)", pkt.src_addr, payload, dbm),
                    None      => log::info!("RX from {}: {}", pkt.src_addr, payload),
                }
                if let Some((origin, readout)) = crate::sportident::CardReadout::parse_payload(&payload) {
                    if relay && origin != own_addr {
                        // Not ours to consume — pass it on toward our own
                        // dest unchanged. The final consumer (whoever that
                        // ends up being) does the buffering; a relay hop
                        // doesn't duplicate that bookkeeping for traffic
                        // that isn't its own.
                        forward(radio.as_mut(), &clients, &state, dest, &payload);
                    } else {
                        state.lock().unwrap().record_punch(origin, pkt.rssi);
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
                        let ack = Frame::PunchAck { node: origin, card_id: readout.card_id };
                        if let Err(e) = radio.send(pkt.src_addr, ack.encode().as_bytes()) {
                            log::error!("failed to ack punch: {}", e);
                        }
                    }
                } else if let Some(frame) = Frame::parse(&payload) {
                    match frame {
                        Frame::Command { target, commander, setting } if target == own_addr => {
                            match setting {
                                Setting::HeartbeatIntervalSecs(secs) => {
                                    heartbeat_period = (secs > 0).then(|| Duration::from_secs(secs as u64));
                                    log::info!("heartbeat interval changed to {}s by command from {}", secs, pkt.src_addr);
                                }
                            }
                            // Ack names the true commander (so a relay
                            // downstream of it knows where to forward this)
                            // but is transmitted to whoever handed us this
                            // packet, same pattern as PunchAck.
                            let ack = Frame::Ack { origin: own_addr, commander, setting };
                            if let Err(e) = radio.send(pkt.src_addr, ack.encode().as_bytes()) {
                                log::error!("failed to ack command: {}", e);
                            }
                            log_and_broadcast(&clients, &state, format!("CMDAPPLIED {:?}", setting));
                        }
                        Frame::Command { target, .. } if relay => {
                            // Not addressed to us — pass it on toward the
                            // named target directly (single-hop relay:
                            // assumed within direct reach downstream).
                            forward(radio.as_mut(), &clients, &state, target, &payload);
                        }
                        Frame::Command { .. } => {
                            // Not addressed to us and not relaying — ignore.
                        }
                        Frame::Ack { origin, commander, setting } if commander == own_addr => {
                            let had = pending_commands.len();
                            pending_commands.retain(|p| !(p.target == origin && p.setting == setting));
                            if pending_commands.len() < had {
                                log::info!("CMD to {} ({:?}) acked", origin, setting);
                                log_and_broadcast(&clients, &state, format!("CMDOK {} {}", origin, setting.encode()));
                            }
                        }
                        Frame::Ack { commander, .. } if relay => {
                            // Not for us — forward toward the commander who
                            // originally issued this command.
                            forward(radio.as_mut(), &clients, &state, commander, &payload);
                        }
                        Frame::Ack { .. } => {
                            // Not relaying and not ours — ignore.
                        }
                        Frame::PunchAck { node, card_id } if node == own_addr => {
                            if pending_punch.as_ref().is_some_and(|p| p.card_id == card_id) {
                                let p = pending_punch.take().unwrap();
                                for id in &p.row_ids {
                                    if let Err(e) = punch_buffer.mark_sent(*id) {
                                        log::error!("failed to mark punch {} sent: {}", id, e);
                                    }
                                }
                                log::info!("PUNCH card {} acked by {}", card_id, pkt.src_addr);
                                log_and_broadcast(&clients, &state, format!("PACKOK {}", card_id));
                            }
                        }
                        Frame::PunchAck { node, .. } if relay => {
                            // Named node isn't us — pass the ack on toward
                            // it directly (single-hop relay: assumed within
                            // direct reach downstream of this relay).
                            forward(radio.as_mut(), &clients, &state, node, &payload);
                        }
                        Frame::PunchAck { .. } => {
                            // Not relaying and not ours — ignore.
                        }
                    }
                } else if let Some(battery) = parse_heartbeat(&payload) {
                    match battery {
                        Some((pct, mv)) => log::info!("HB from {}: battery {}% ({}mV)", pkt.src_addr, pct, mv),
                        None => log::info!("HB from {} (no battery data)", pkt.src_addr),
                    }
                    // Heartbeats don't carry an origin field yet (they can't
                    // relay — see docs/protocols/lora_online_control_protocol.md),
                    // so pkt.src_addr is always the true origin here, unlike
                    // the punch/command branches which use an explicit
                    // `origin`/`commander` field for exactly this reason.
                    state.lock().unwrap().record_heartbeat(pkt.src_addr, battery);
                    let battery_field = battery
                        .map(|(pct, mv)| format!("{} {}", pct, mv))
                        .unwrap_or_else(|| "-".to_string());
                    log_and_broadcast(&clients, &state, format!("HBRX {} {}", pkt.src_addr, battery_field));
                }
                log_and_broadcast(&clients, &state, format!("RX {} {} {}", pkt.src_addr, rssi_str, payload));
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("RX error: {}", e);
                log_and_broadcast(&clients, &state, format!("ERR RX: {}", e));
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── Attach mode ───────────────────────────────────────────────────────────────

struct SocketRadio {
    writer: BufWriter<std::os::unix::net::UnixStream>,
    events: std::sync::mpsc::Receiver<ReceivedPacket>,
    status_events: std::sync::mpsc::Receiver<StatusEvent>,
}

impl SocketRadio {
    fn new(stream: std::os::unix::net::UnixStream) -> Result<Self> {
        let read_stream = stream.try_clone()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let (status_tx, status_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let reader = BufReader::new(read_stream);
            for line in reader.lines().flatten() {
                if let Some(pkt) = parse_rx_line(&line) {
                    if tx.send(pkt).is_err() {
                        break;
                    }
                } else if let Some(evt) = parse_status_line(&line) {
                    if status_tx.send(evt).is_err() {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            writer: BufWriter::new(stream),
            events: rx,
            status_events: status_rx,
        })
    }
}

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
    None
}

impl Radio for SocketRadio {
    fn send(&mut self, _dest: u16, payload: &[u8]) -> Result<()> {
        writeln!(self.writer, "SEND {}", String::from_utf8_lossy(payload))?;
        self.writer.flush()?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        Ok(self.events.try_recv().ok())
    }

    fn set_dest(&mut self, dest: u16) -> Result<()> {
        writeln!(self.writer, "SET_DEST {}", dest)?;
        self.writer.flush()?;
        Ok(())
    }

    fn poll_status(&mut self) -> Vec<StatusEvent> {
        self.status_events.try_iter().collect()
    }

    fn send_command(&mut self, _commander: u16, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
        // The daemon fills in its own address as commander when it builds
        // the actual radio frame — an attach client has no radio identity
        // of its own to offer here.
        writeln!(self.writer, "CMD {} {}", target, heartbeat_interval_secs)?;
        self.writer.flush()?;
        Ok(())
    }
}

pub fn attach(socket_path: &str, addr: u16, dest: u16) -> Result<()> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot connect to daemon at {}: {}", socket_path, e))?;

    let radio = SocketRadio::new(stream)?;
    let (_, si_rx) = std::sync::mpsc::channel();
    crate::ui::run_app("attached to daemon".to_string(), addr, dest, Box::new(radio), 0, si_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_local_batch_empty_buffer_is_none() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        assert!(next_local_batch(&buffer, 5).unwrap().is_none());
    }

    #[test]
    fn test_next_local_batch_groups_same_card_id_and_matches_wire_format() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        let id1 = buffer.record(123456, 31, 36070, "local").unwrap();
        let id2 = buffer.record(123456, 32, 36200, "local").unwrap();

        let (card_id, row_ids, payload) = next_local_batch(&buffer, 5).unwrap().unwrap();
        assert_eq!(card_id, 123456);
        assert_eq!(row_ids, vec![id1, id2]);

        let expected = crate::sportident::CardReadout {
            card_id: 123456,
            punches: vec![
                crate::sportident::ControlPunch { station: 31, time_s: 36070 },
                crate::sportident::ControlPunch { station: 32, time_s: 36200 },
            ],
        }.to_payload(5);
        assert_eq!(payload, expected);
    }

    #[test]
    fn test_next_local_batch_does_not_mix_different_card_ids() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        buffer.record(1, 1, 100, "local").unwrap();
        buffer.record(2, 2, 200, "local").unwrap();

        let (card_id, row_ids, _) = next_local_batch(&buffer, 5).unwrap().unwrap();
        assert_eq!(card_id, 1);
        assert_eq!(row_ids.len(), 1);
    }

    #[test]
    fn test_next_local_batch_ignores_remote_sourced_rows() {
        let buffer = crate::punch_buffer::PunchBuffer::open(":memory:").unwrap();
        buffer.record(1, 1, 100, "192.168.1.5").unwrap();
        let id = buffer.record(2, 2, 200, "local").unwrap();

        let (card_id, row_ids, _) = next_local_batch(&buffer, 5).unwrap().unwrap();
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

    /// Real end-to-end check of the client -> daemon wire protocol for
    /// commands: a SocketRadio wrapping one end of a real Unix socket pair
    /// should write exactly "CMD <target> <secs>" to the other end when
    /// send_command is called, since that's the line the daemon's
    /// handle_client / run_daemon_loop parse to originate a command.
    #[test]
    fn test_socket_radio_send_command_writes_expected_line() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut radio = SocketRadio::new(a).unwrap();
        radio.send_command(1, 5, 30).unwrap();

        let mut reader = BufReader::new(b);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), "CMD 5 30");
    }

    #[test]
    fn test_parse_heartbeat_bare() {
        assert_eq!(parse_heartbeat("HB"), Some(None));
    }

    #[test]
    fn test_parse_heartbeat_with_battery() {
        assert_eq!(parse_heartbeat("HB 82 3950"), Some(Some((82, 3950))));
    }

    #[test]
    fn test_parse_heartbeat_rejects_non_heartbeat() {
        assert_eq!(parse_heartbeat("PUNCH 5 123456 31:100"), None);
        assert_eq!(parse_heartbeat(""), None);
        assert_eq!(parse_heartbeat("HB notanumber 3950"), None);
        assert_eq!(parse_heartbeat("HB 82"), None);
    }

    /// A radio double that just records what was sent and lets a test queue
    /// up canned received packets — enough to test NetworkFilteredRadio's
    /// prepend/strip logic without any real hardware.
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

    fn packet(src_addr: u16, payload: &str) -> ReceivedPacket {
        let mut p = heapless::Vec::<u8, 240>::new();
        let _ = p.extend_from_slice(payload.as_bytes());
        ReceivedPacket { src_addr, payload: p, rssi: None }
    }

    #[test]
    fn test_network_filtered_radio_prepends_network_id_on_send() {
        let inner = FakeRadio { sent: Vec::new(), to_receive: Default::default() };
        let mut radio = NetworkFilteredRadio::new(inner, "LOC".to_string());
        radio.send(5, b"PUNCH 10 123456 31:100").unwrap();
        assert_eq!(radio.inner.sent, vec![(5, b"LOC PUNCH 10 123456 31:100".to_vec())]);
    }

    #[test]
    fn test_network_filtered_radio_strips_matching_network_id_on_receive() {
        let mut inner = FakeRadio { sent: Vec::new(), to_receive: Default::default() };
        inner.to_receive.push_back(packet(5, "LOC PUNCH 10 123456 31:100"));
        let mut radio = NetworkFilteredRadio::new(inner, "LOC".to_string());

        let pkt = radio.receive().unwrap().unwrap();
        assert_eq!(pkt.payload.as_slice(), b"PUNCH 10 123456 31:100");
        assert_eq!(pkt.src_addr, 5);
    }

    #[test]
    fn test_network_filtered_radio_drops_mismatched_network_id() {
        let mut inner = FakeRadio { sent: Vec::new(), to_receive: Default::default() };
        inner.to_receive.push_back(packet(5, "OTHERDEPLOY PUNCH 10 123456 31:100"));
        let mut radio = NetworkFilteredRadio::new(inner, "LOC".to_string());

        assert!(radio.receive().unwrap().is_none());
    }

    #[test]
    fn test_network_filtered_radio_drops_payload_with_no_network_id_at_all() {
        let mut inner = FakeRadio { sent: Vec::new(), to_receive: Default::default() };
        inner.to_receive.push_back(packet(5, "PUNCH 10 123456 31:100"));
        let mut radio = NetworkFilteredRadio::new(inner, "LOC".to_string());

        assert!(radio.receive().unwrap().is_none());
    }
}
