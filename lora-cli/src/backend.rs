use anyhow::Result;
use sx126x::{Config, NoPin, ReceivedPacket, Sx126xUart, LoraRadio};
use std::io::{BufRead, BufReader, BufWriter, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

pub trait Radio: Send {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<ReceivedPacket>>;
    fn set_dest(&mut self, _dest: u16) -> Result<()> { Ok(()) }
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

fn open_serial(port: &str) -> Result<SerialPortWrapper> {
    let s = serialport::new(port, 9600)
        .timeout(std::time::Duration::from_millis(1000))
        .open()
        .map_err(|e| anyhow::anyhow!("failed to open {}: {}", port, e))?;
    Ok(SerialPortWrapper(s))
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> Result<()> {
    let config = build_config(&args)?;

    #[cfg(feature = "rpi")]
    if args.m0_pin.is_some() && args.m1_pin.is_some() {
        return run_rpi(args, config);
    }

    run_desktop(args, config)
}

fn run_desktop(args: Args, config: Config) -> Result<()> {
    let port = args.port.as_deref().unwrap_or("").to_string();

    if std::io::stdout().is_terminal() {
        let serial = open_serial(&port)?;
        let mut driver = Sx126xUart::new(serial, NoPin, NoPin, StdDelay);
        driver.configure(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
        let port_info = format!("port: {}  freq: {}MHz  power: {:?}", port, config.freq_mhz, config.power);
        return crate::ui::run_app(port_info, args.addr, args.dest, Box::new(driver), args.heartbeat_interval);
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
        match driver.configure(&config) {
            Ok(()) => {
                let ack = &driver.last_configure_ack;
                let addr = ((ack[3] as u16) << 8) | ack[4] as u16;
                log::info!("module configure ACK: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                log::info!("module ready");
                break Box::new(driver);
            }
            Err(e) => {
                log::warn!("module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    run_daemon_loop(args.dest, args.heartbeat_interval, clients, cmd_rx, radio)
}

#[cfg(feature = "rpi")]
fn run_rpi(args: Args, config: Config) -> Result<()> {
    use rppal::gpio::Gpio;

    struct RppalPin(rppal::gpio::OutputPin);

    impl embedded_hal::digital::ErrorType for RppalPin {
        type Error = std::convert::Infallible;
    }
    impl embedded_hal::digital::OutputPin for RppalPin {
        fn set_high(&mut self) -> Result<(), Self::Error> { self.0.set_high(); Ok(()) }
        fn set_low(&mut self) -> Result<(), Self::Error> { self.0.set_low(); Ok(()) }
    }

    let port = args.port.as_deref().unwrap_or("").to_string();
    let m0_pin = args.m0_pin.unwrap();
    let m1_pin = args.m1_pin.unwrap();

    if std::io::stdout().is_terminal() {
        let gpio = Gpio::new()?;
        let m0 = RppalPin(gpio.get(m0_pin)?.into_output_low());
        let m1 = RppalPin(gpio.get(m1_pin)?.into_output_low());
        let serial = open_serial(&port)?;
        let mut driver = Sx126xUart::new(serial, m0, m1, StdDelay);
        driver.configure(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
        let port_info = format!("port: {}  freq: {}MHz  power: {:?}", port, config.freq_mhz, config.power);
        return crate::ui::run_app(port_info, args.addr, args.dest, Box::new(driver), args.heartbeat_interval);
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
        match driver.configure(&config) {
            Ok(()) => {
                let ack = &driver.last_configure_ack;
                let addr = ((ack[3] as u16) << 8) | ack[4] as u16;
                log::info!("module configure ACK: addr={} ch={} regs={:02X?}", addr, ack[8], ack);
                log::info!("module ready");
                break Box::new(driver);
            }
            Err(e) => {
                log::warn!("module not responding ({}), retrying in 5s", e);
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    run_daemon_loop(args.dest, args.heartbeat_interval, clients, cmd_rx, radio)
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
            if line.starts_with("SEND ") || line.starts_with("SET_DEST ") {
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

fn run_daemon_loop(
    dest: u16,
    heartbeat_interval: u64,
    clients: Clients,
    cmd_rx: std::sync::mpsc::Receiver<String>,
    mut radio: Box<dyn Radio>,
) -> Result<()> {
    let heartbeat_period = (heartbeat_interval > 0).then(|| Duration::from_secs(heartbeat_interval));
    let mut last_heartbeat = Instant::now();
    let mut dest = dest;

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
            }
        }

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

        match radio.receive() {
            Ok(Some(pkt)) => {
                let payload = String::from_utf8_lossy(&pkt.payload).into_owned();
                let rssi_str = pkt.rssi.map(|r| r.to_string()).unwrap_or_else(|| "-".to_string());
                match pkt.rssi {
                    Some(dbm) => log::info!("RX from {}: {} (RSSI: {}dBm)", pkt.src_addr, payload, dbm),
                    None      => log::info!("RX from {}: {}", pkt.src_addr, payload),
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
}

impl SocketRadio {
    fn new(stream: std::os::unix::net::UnixStream) -> Result<Self> {
        let read_stream = stream.try_clone()?;
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let reader = BufReader::new(read_stream);
            for line in reader.lines().flatten() {
                if let Some(pkt) = parse_rx_line(&line) {
                    if tx.send(pkt).is_err() {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            writer: BufWriter::new(stream),
            events: rx,
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
}

pub fn attach(socket_path: &str, addr: u16, dest: u16) -> Result<()> {
    let stream = std::os::unix::net::UnixStream::connect(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot connect to daemon at {}: {}", socket_path, e))?;

    let radio = SocketRadio::new(stream)?;
    crate::ui::run_app("attached to daemon".to_string(), addr, dest, Box::new(radio), 0)
}
