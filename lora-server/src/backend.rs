use anyhow::Result;
use sx126x::{Config, NoPin, ReceivedPacket, Sx126xUart, LoraRadio};
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

// ── Serial wrapper ────────────────────────────────────────────────────────────

pub struct SerialPortWrapper(pub Box<dyn serialport::SerialPort>);

impl embedded_io::ErrorType for SerialPortWrapper {
    type Error = embedded_io::ErrorKind;
}

impl embedded_io::Read for SerialPortWrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Read::read(&mut self.0, buf).map_err(|e| e.kind().into())
    }
}

impl embedded_io::Write for SerialPortWrapper {
    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Write::write(&mut self.0, buf).map_err(|e| e.kind().into())
    }
    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        std::io::Write::flush(&mut self.0).map_err(|e| e.kind().into())
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

    /// Ask the node at `target` to change its heartbeat interval. Direct
    /// hardware backends send this as a single, untracked frame — a human
    /// watching the TUI can retry manually if no ack shows up. The daemon's
    /// SocketRadio instead hands this off to the daemon itself, which owns
    /// retry-with-ack tracking (see run_daemon_loop).
    fn send_command(&mut self, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
        let frame = Frame::Command { target, setting: Setting::HeartbeatIntervalSecs(heartbeat_interval_secs) };
        self.send(target, frame.encode().as_bytes())
    }
}

impl<R: LoraRadio + Send> Radio for R
where
    R::Error: std::error::Error + Send + Sync + 'static,
{
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
        LoraRadio::send(self, dest, payload).map_err(anyhow::Error::from)
    }
    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        LoraRadio::receive(self).map_err(anyhow::Error::from)
    }
}

// ── Config builder ────────────────────────────────────────────────────────────

fn build_config(args: &Args) -> Result<Config> {
    use sx126x::{AirSpeed, BufferSize, TxPower};

    let power = match args.power {
        22 => TxPower::Dbm22,
        17 => TxPower::Dbm17,
        13 => TxPower::Dbm13,
        10 => TxPower::Dbm10,
        p  => anyhow::bail!("unsupported power {}dBm — use 10, 13, 17, or 22", p),
    };
    let air_speed = match args.air_speed {
        1200  => AirSpeed::Bps1200,
        2400  => AirSpeed::Bps2400,
        4800  => AirSpeed::Bps4800,
        9600  => AirSpeed::Bps9600,
        19200 => AirSpeed::Bps19200,
        38400 => AirSpeed::Bps38400,
        62500 => AirSpeed::Bps62500,
        s => anyhow::bail!("unsupported air_speed {} bps", s),
    };
    Ok(Config {
        freq_mhz: args.freq,
        addr: args.addr,
        net_id: 0,
        power,
        air_speed,
        buffer_size: BufferSize::Bytes240,
        rssi: true,
        crypt: 0,
    })
}

#[cfg(feature = "rpi")]
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

// Local newtype: sx126x::LoraRadio and sx127x::Sx127xSpi are both foreign to
// this crate, so a direct `impl Radio for Sx127xSpi<..>` conflicts (in the
// compiler's eyes) with the sx126x blanket impl above — a future sx127x
// version could add `impl sx126x::LoraRadio for Sx127xSpi`, which neither
// impl can rule out. Wrapping in a local type sidesteps that.
#[cfg(feature = "rpi")]
struct Sx127xRadio<SPI, RESET, DELAY>(sx127x::Sx127xSpi<SPI, RESET, DELAY>);

#[cfg(feature = "rpi")]
impl<SPI, RESET, DELAY> Radio for Sx127xRadio<SPI, RESET, DELAY>
where
    SPI: embedded_hal::spi::SpiDevice + Send,
    RESET: embedded_hal::digital::OutputPin + Send,
    DELAY: embedded_hal::delay::DelayNs + Send,
{
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()> {
        sx127x::LoraRadio::send(&mut self.0, dest, payload).map_err(|e| anyhow::anyhow!("{}", e))
    }
    fn receive(&mut self) -> Result<Option<ReceivedPacket>> {
        sx127x::LoraRadio::receive(&mut self.0).map_err(|e| anyhow::anyhow!("{}", e))
    }
}

fn open_serial(port: &str) -> Result<SerialPortWrapper> {
    let s = serialport::new(port, 9600)
        .timeout(std::time::Duration::from_millis(1000))
        .open()
        .map_err(|e| anyhow::anyhow!("failed to open {}: {}", port, e))?;
    Ok(SerialPortWrapper(s))
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> Result<()> {
    if args.radio == "spi" {
        #[cfg(feature = "rpi")]
        return run_spi(args);
        #[cfg(not(feature = "rpi"))]
        anyhow::bail!("--radio spi requires building lora-server with the `rpi` feature");
    }

    let config = build_config(&args)?;

    #[cfg(feature = "rpi")]
    if args.m0_pin.is_some() && args.m1_pin.is_some() {
        return run_rpi(args, config);
    }

    run_desktop(args, config)
}

fn run_desktop(args: Args, config: Config) -> Result<()> {
    let port = args.port.as_deref().unwrap_or("").to_string();
    let si_rx = crate::sportident::spawn_si_worker();

    if std::io::stdout().is_terminal() {
        let serial = open_serial(&port)?;
        let mut driver = Sx126xUart::new(serial, NoPin, NoPin, StdDelay);
        driver.configure_if_needed(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
        let port_info = format!("port: {}  freq: {}MHz  power: {:?}", port, config.freq_mhz, config.power);
        return crate::ui::run_app(port_info, args.addr, args.dest, Box::new(driver), args.heartbeat_interval, si_rx);
    }

    // Daemon: create socket first so clients can connect while waiting for hardware.
    let (clients, cmd_rx) = setup_daemon_socket(&args.socket)?;

    let radio: Box<dyn Radio> = loop {
        let serial = match open_serial(&port) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("cannot open serial port ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let mut driver = Sx126xUart::new(serial, NoPin, NoPin, StdDelay);
        match driver.configure_if_needed(&config) {
            Ok(written) => {
                let ack = &driver.last_configure_ack;
                let addr = ((ack[3] as u16) << 8) | ack[4] as u16;
                if written {
                    log::info!("module configured: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                } else {
                    log::info!("module config unchanged: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                }
                log::info!("module ready");
                break Box::new(driver);
            }
            Err(e) => {
                log::warn!("module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let punch_buffer = setup_punch_pipeline(&args)?;
    run_daemon_loop(
        DaemonIdentity { own_addr: args.addr, dest: args.dest, heartbeat_interval: args.heartbeat_interval },
        clients, cmd_rx, radio, si_rx, punch_buffer,
    )
}

#[cfg(feature = "rpi")]
struct RppalPin(rppal::gpio::OutputPin);

#[cfg(feature = "rpi")]
impl embedded_hal::digital::ErrorType for RppalPin {
    type Error = std::convert::Infallible;
}
#[cfg(feature = "rpi")]
impl embedded_hal::digital::OutputPin for RppalPin {
    fn set_high(&mut self) -> Result<(), Self::Error> { self.0.set_high(); Ok(()) }
    fn set_low(&mut self) -> Result<(), Self::Error> { self.0.set_low(); Ok(()) }
}

#[cfg(feature = "rpi")]
fn run_rpi(args: Args, config: Config) -> Result<()> {
    use rppal::gpio::Gpio;

    let port = args.port.as_deref().unwrap_or("").to_string();
    let m0_pin = args.m0_pin.unwrap();
    let m1_pin = args.m1_pin.unwrap();
    let si_rx = crate::sportident::spawn_si_worker();

    if std::io::stdout().is_terminal() {
        let gpio = Gpio::new()?;
        let m0 = RppalPin(gpio.get(m0_pin)?.into_output_low());
        let m1 = RppalPin(gpio.get(m1_pin)?.into_output_low());
        let serial = open_serial(&port)?;
        let mut driver = Sx126xUart::new(serial, m0, m1, StdDelay);
        driver.configure_if_needed(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
        let port_info = format!("port: {}  freq: {}MHz  power: {:?}", port, config.freq_mhz, config.power);
        return crate::ui::run_app(port_info, args.addr, args.dest, Box::new(driver), args.heartbeat_interval, si_rx);
    }

    // Daemon: socket first, then retry hardware.
    let (clients, cmd_rx) = setup_daemon_socket(&args.socket)?;

    let radio: Box<dyn Radio> = loop {
        let gpio = match Gpio::new() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("GPIO error ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let serial = match open_serial(&port) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("cannot open serial port ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let m0 = match gpio.get(m0_pin) {
            Ok(p) => RppalPin(p.into_output_low()),
            Err(e) => {
                log::warn!("GPIO pin error ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let m1 = RppalPin(gpio.get(m1_pin).unwrap().into_output_low());
        let mut driver = Sx126xUart::new(serial, m0, m1, StdDelay);
        match driver.configure_if_needed(&config) {
            Ok(written) => {
                let ack = &driver.last_configure_ack;
                let addr = ((ack[3] as u16) << 8) | ack[4] as u16;
                if written {
                    log::info!("module configured: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                } else {
                    log::info!("module config unchanged: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                }
                log::info!("module ready");
                break Box::new(driver);
            }
            Err(e) => {
                log::warn!("module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let punch_buffer = setup_punch_pipeline(&args)?;
    run_daemon_loop(
        DaemonIdentity { own_addr: args.addr, dest: args.dest, heartbeat_interval: args.heartbeat_interval },
        clients, cmd_rx, radio, si_rx, punch_buffer,
    )
}

#[cfg(feature = "rpi")]
fn run_spi(args: Args) -> Result<()> {
    use rppal::gpio::Gpio;
    use rppal::spi::{Bus, Mode, SimpleHalSpiDevice, SlaveSelect, Spi};

    let config = build_sx127x_config(&args)?;
    let si_rx = crate::sportident::spawn_si_worker();

    let build_driver = || -> Result<Sx127xRadio<SimpleHalSpiDevice<Spi>, RppalPin, StdDelay>> {
        let spi = Spi::new(Bus::Spi0, SlaveSelect::Ss0, 1_000_000, Mode::Mode0)
            .map_err(|e| anyhow::anyhow!("SPI open failed: {}", e))?;
        let spi_device = SimpleHalSpiDevice::new(spi);
        let gpio = Gpio::new()?;
        let reset = RppalPin(gpio.get(args.reset_pin)?.into_output_high());
        let mut driver = sx127x::Sx127xSpi::new(spi_device, reset, StdDelay);
        sx127x::LoraRadio::configure(&mut driver, &config).map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(Sx127xRadio(driver))
    };

    if std::io::stdout().is_terminal() {
        let driver = build_driver()?;
        let port_info = format!(
            "SPI0 CE0  freq: {}Hz  sf: {}  bw: {}Hz",
            config.freq_hz, config.spreading_factor, config.bandwidth.hz()
        );
        return crate::ui::run_app(port_info, args.addr, args.dest, Box::new(driver), args.heartbeat_interval, si_rx);
    }

    let (clients, cmd_rx) = setup_daemon_socket(&args.socket)?;
    let radio: Box<dyn Radio> = loop {
        match build_driver() {
            Ok(driver) => {
                log::info!("SX1276 module ready on SPI0 CE0");
                break Box::new(driver);
            }
            Err(e) => {
                log::warn!("SPI module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    let punch_buffer = setup_punch_pipeline(&args)?;
    run_daemon_loop(
        DaemonIdentity { own_addr: args.addr, dest: args.dest, heartbeat_interval: args.heartbeat_interval },
        clients, cmd_rx, radio, si_rx, punch_buffer,
    )
}

// ── Daemon socket + loop ──────────────────────────────────────────────────────

type Clients = Arc<Mutex<Vec<std::sync::mpsc::SyncSender<String>>>>;

fn broadcast(clients: &Clients, msg: String) {
    let mut guard = clients.lock().unwrap();
    guard.retain(|tx| tx.try_send(msg.clone()).is_ok());
}

fn setup_daemon_socket(
    socket_path: &str,
) -> Result<(Clients, std::sync::mpsc::Receiver<String>)> {
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

    let listener_clients = Arc::clone(&clients);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let c = Arc::clone(&listener_clients);
            let tx = cmd_tx.clone();
            std::thread::spawn(move || handle_client(stream, c, tx));
        }
    });

    log::info!("socket ready at {}", socket_path);
    Ok((clients, cmd_rx))
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
            if line.starts_with("SEND ") || line.starts_with("SET_DEST ") || line.starts_with("CMD ") {
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

fn send_command_frame(radio: &mut dyn Radio, clients: &Clients, target: u16, setting: Setting) {
    let frame = Frame::Command { target, setting };
    let payload = frame.encode();
    match radio.send(target, payload.as_bytes()) {
        Ok(()) => {
            log::info!("CMD to {}: {}", target, payload);
            broadcast(clients, format!("TX {} {}", target, payload));
        }
        Err(e) => {
            log::error!("CMD send failed: {}", e);
            broadcast(clients, format!("ERR CMD: {}", e));
        }
    }
}

/// The daemon's own identity and starting config — as opposed to the
/// runtime handles (radio, sockets, buffers) it operates on.
struct DaemonIdentity {
    own_addr: u16,
    dest: u16,
    heartbeat_interval: u64,
}

fn run_daemon_loop(
    identity: DaemonIdentity,
    clients: Clients,
    cmd_rx: std::sync::mpsc::Receiver<String>,
    mut radio: Box<dyn Radio>,
    si_rx: std::sync::mpsc::Receiver<crate::sportident::CardReadout>,
    punch_buffer: Arc<crate::punch_buffer::PunchBuffer>,
) -> Result<()> {
    let DaemonIdentity { own_addr, dest, heartbeat_interval } = identity;
    let mut heartbeat_period = (heartbeat_interval > 0).then(|| Duration::from_secs(heartbeat_interval));
    let mut last_heartbeat = Instant::now();
    let mut dest = dest;
    let mut pending_commands: Vec<PendingCommand> = Vec::new();

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
                        broadcast(&clients, format!("TX {} {}", dest, payload));
                    }
                    Err(e) => {
                        log::error!("TX failed: {}", e);
                        broadcast(&clients, format!("ERR TX: {}", e));
                    }
                }
            } else if let Some(rest) = cmd.strip_prefix("CMD ") {
                let mut parts = rest.splitn(2, ' ');
                if let (Some(target_str), Some(secs_str)) = (parts.next(), parts.next()) {
                    if let (Ok(target), Ok(secs)) = (target_str.parse::<u16>(), secs_str.parse::<u32>()) {
                        let setting = Setting::HeartbeatIntervalSecs(secs);
                        send_command_frame(radio.as_mut(), &clients, target, setting);
                        pending_commands.push(PendingCommand { target, setting, sent_at: Instant::now(), attempts: 1 });
                    }
                }
            }
        }

        pending_commands.retain_mut(|cmd| {
            if cmd.sent_at.elapsed() < CMD_RETRY_INTERVAL {
                return true;
            }
            if cmd.attempts >= CMD_MAX_ATTEMPTS {
                log::warn!("CMD to {} ({:?}) gave up after {} attempts", cmd.target, cmd.setting, cmd.attempts);
                broadcast(&clients, format!("CMDERR {} {}", cmd.target, cmd.setting.encode()));
                return false;
            }
            cmd.attempts += 1;
            cmd.sent_at = Instant::now();
            send_command_frame(radio.as_mut(), &clients, cmd.target, cmd.setting);
            true
        });

        if let Some(period) = heartbeat_period {
            if last_heartbeat.elapsed() >= period {
                last_heartbeat = Instant::now();
                match radio.send(dest, b"HB") {
                    Ok(()) => {
                        log::info!("HB sent to {}", dest);
                        broadcast(&clients, format!("HB {}", dest));
                    }
                    Err(e) => {
                        log::error!("HB send failed: {}", e);
                        broadcast(&clients, format!("ERR HB: {}", e));
                    }
                }
            }
        }

        while let Ok(readout) = si_rx.try_recv() {
            for p in &readout.punches {
                if let Err(e) = punch_buffer.record(readout.card_id, p.station, p.time_s, "local") {
                    log::error!("failed to buffer local punch: {}", e);
                }
            }
            let msg = readout.to_payload();
            match radio.send(dest, msg.as_bytes()) {
                Ok(()) => {
                    log::info!("TX to {}: {}", dest, msg);
                    broadcast(&clients, format!("TX {} {}", dest, msg));
                }
                Err(e) => {
                    log::error!("SI TX failed: {}", e);
                    broadcast(&clients, format!("ERR TX: {}", e));
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
                if let Some(readout) = crate::sportident::CardReadout::parse_payload(&payload) {
                    let source = pkt.src_addr.to_string();
                    for p in &readout.punches {
                        if let Err(e) = punch_buffer.record(readout.card_id, p.station, p.time_s, &source) {
                            log::error!("failed to buffer remote punch: {}", e);
                        }
                    }
                } else if let Some(frame) = Frame::parse(&payload) {
                    match frame {
                        Frame::Command { target, setting } if target == own_addr => {
                            match setting {
                                Setting::HeartbeatIntervalSecs(secs) => {
                                    heartbeat_period = (secs > 0).then(|| Duration::from_secs(secs as u64));
                                    log::info!("heartbeat interval changed to {}s by command from {}", secs, pkt.src_addr);
                                }
                            }
                            let ack = Frame::Ack { origin: own_addr, setting };
                            if let Err(e) = radio.send(pkt.src_addr, ack.encode().as_bytes()) {
                                log::error!("failed to ack command: {}", e);
                            }
                            broadcast(&clients, format!("CMDAPPLIED {:?}", setting));
                        }
                        Frame::Command { .. } => {
                            // Not addressed to us and we don't relay yet — ignore.
                        }
                        Frame::Ack { origin, setting } => {
                            let had = pending_commands.len();
                            pending_commands.retain(|p| !(p.target == origin && p.setting == setting));
                            if pending_commands.len() < had {
                                log::info!("CMD to {} ({:?}) acked", origin, setting);
                                broadcast(&clients, format!("CMDOK {} {}", origin, setting.encode()));
                            }
                        }
                    }
                }
                broadcast(&clients, format!("RX {} {} {}", pkt.src_addr, rssi_str, payload));
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("RX error: {}", e);
                broadcast(&clients, format!("ERR RX: {}", e));
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

    fn send_command(&mut self, target: u16, heartbeat_interval_secs: u32) -> Result<()> {
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
        radio.send_command(5, 30).unwrap();

        let mut reader = BufReader::new(b);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), "CMD 5 30");
    }
}
