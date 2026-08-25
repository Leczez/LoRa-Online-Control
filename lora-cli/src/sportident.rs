use std::io::Read as _;
use std::time::Duration;
use anyhow::Result;

// ─── USB device identity ───────────────────────────────────────────────────────

pub const SI_VID: u16 = 0x10C4;  // Silicon Labs CP210x
pub const SI_PID: u16 = 0x800A;  // SportIdent BSM7/BSM8-USB

const SI_BAUD: u32 = 38400;

// ─── Protocol constants ────────────────────────────────────────────────────────

const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const WAKEUP: u8 = 0xFF;

// Unsolicited messages sent by the station when a card is presented
const C_SI5_DET: u8 = 0xE5;
const C_SI6_DET: u8 = 0xE6;
const C_SI9_DET: u8 = 0xE8;  // SI-Card 8/9/10/11/p/t
const C_SI_REM: u8 = 0xE7;

// Commands sent to the station to read card data
const C_GET_SI5: u8 = 0xB1;
const C_GET_SI6: u8 = 0xE1;
const C_GET_SI9: u8 = 0xEF;

// Offsets within the 128-byte SI Card 9 image returned by C_GET_SI9
// Verify these against sireader2.py if the hardware behaves differently.
const SI9_PUNCH_COUNT_OFFSET: usize = 0x16;
const SI9_PUNCH_START_OFFSET: usize = 0x38;  // 4 bytes each: [station, td, th, tl]

// ─── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ControlPunch {
    pub station: u8,
    /// Seconds since midnight (12-hour cycle; AM/PM resolved via td byte).
    pub time_s: u32,
}

#[derive(Debug, Clone)]
pub struct CardReadout {
    pub card_id: u32,
    pub punches: Vec<ControlPunch>,
}

impl CardReadout {
    /// Wire format sent over LoRa: `PUNCH <card_id> <station>:<time_s>,...`
    pub fn to_payload(&self) -> String {
        let punches: String = self.punches.iter()
            .map(|p| format!("{}:{}", p.station, p.time_s))
            .collect::<Vec<_>>()
            .join(",");
        format!("PUNCH {} {}", self.card_id, punches)
    }
}

#[derive(Debug)]
pub enum SiEvent {
    CardReadout(CardReadout),
    CardRemoved,
}

// ─── Internal types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum CardType { Si5, Si6, Si9 }

#[derive(Debug)]
enum ParsedPacket {
    CardInserted { card_type: CardType, card_id: u32 },
    CardRemoved,
    CardData { data: Vec<u8> },
}

// ─── CRC-16 (polynomial 0x8005, covering CMD+LEN+DATA) ────────────────────────

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &b| {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
        crc
    })
}

fn build_command(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut pkt = vec![WAKEUP, STX, cmd, data.len() as u8];
    pkt.extend_from_slice(data);
    let crc = crc16(&pkt[2..]);  // CRC over CMD + LEN + DATA
    pkt.extend_from_slice(&[(crc >> 8) as u8, crc as u8]);
    pkt.push(ETX);
    pkt
}

// ─── SI card reader ────────────────────────────────────────────────────────────

pub struct SiReader {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
    pending: Option<(CardType, u32)>,
}

impl SiReader {
    pub fn open(port_path: &str) -> Result<Self> {
        let port = serialport::new(port_path, SI_BAUD)
            .timeout(Duration::from_millis(100))
            .open()?;
        Ok(Self { port, buf: Vec::new(), pending: None })
    }

    fn send(&mut self, cmd: u8, data: &[u8]) -> Result<()> {
        use std::io::Write as _;
        self.port.write_all(&build_command(cmd, data))?;
        Ok(())
    }

    fn try_parse_packet(&mut self) -> Option<ParsedPacket> {
        loop {
            let stx = self.buf.iter().position(|&b| b == STX)?;
            self.buf.drain(..stx);

            if self.buf.len() < 3 { return None; }

            let cmd = self.buf[1];
            let len = self.buf[2] as usize;
            let total = 1 + 1 + 1 + len + 2 + 1;  // STX CMD LEN DATA[len] CRC[2] ETX

            if self.buf.len() < total { return None; }

            if self.buf[total - 1] != ETX {
                self.buf.remove(0);
                continue;
            }

            let data = self.buf[3..3 + len].to_vec();
            self.buf.drain(..total);

            match cmd {
                C_SI5_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si5,
                        card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI6_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si6,
                        card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI9_DET if data.len() >= 4 => {
                    // Byte 0 is a status/recheck byte; card number is in bytes 1-3.
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si9,
                        card_id: card_id_3b(&data[1..4]),
                    });
                }
                C_SI_REM => return Some(ParsedPacket::CardRemoved),
                C_GET_SI5 | C_GET_SI6 | C_GET_SI9 => {
                    return Some(ParsedPacket::CardData { data });
                }
                _ => continue,  // unknown command, look for next packet
            }
        }
    }

    /// Read bytes from the port and return the next complete SI event, if any.
    /// Returns `Ok(None)` on timeout (no data yet).
    pub fn read_event(&mut self) -> Result<Option<SiEvent>> {
        let mut tmp = [0u8; 256];
        match self.port.read(&mut tmp) {
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }

        while let Some(pkt) = self.try_parse_packet() {
            match pkt {
                ParsedPacket::CardInserted { card_type, card_id } => {
                    self.pending = Some((card_type, card_id));
                    let get_cmd = match card_type {
                        CardType::Si5 => C_GET_SI5,
                        CardType::Si6 => C_GET_SI6,
                        CardType::Si9 => C_GET_SI9,
                    };
                    self.send(get_cmd, &[])?;
                }
                ParsedPacket::CardData { data } => {
                    if let Some((card_type, card_id)) = self.pending.take() {
                        if let Some(readout) = parse_card_data(card_type, card_id, &data) {
                            return Ok(Some(SiEvent::CardReadout(readout)));
                        }
                    }
                }
                ParsedPacket::CardRemoved => {
                    self.pending = None;
                    return Ok(Some(SiEvent::CardRemoved));
                }
            }
        }

        Ok(None)
    }
}

// ─── Card data parsing ─────────────────────────────────────────────────────────

fn card_id_3b(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

fn parse_card_data(card_type: CardType, card_id: u32, data: &[u8]) -> Option<CardReadout> {
    match card_type {
        CardType::Si9 => parse_si9(card_id, data),
        CardType::Si5 | CardType::Si6 => Some(CardReadout { card_id, punches: vec![] }),
    }
}

fn parse_si9(card_id: u32, data: &[u8]) -> Option<CardReadout> {
    if data.len() <= SI9_PUNCH_START_OFFSET { return None; }

    let punch_count = *data.get(SI9_PUNCH_COUNT_OFFSET)? as usize;
    let max = (data.len() - SI9_PUNCH_START_OFFSET) / 4;
    let count = punch_count.min(max);

    let mut punches = Vec::with_capacity(count);
    for i in 0..count {
        let off = SI9_PUNCH_START_OFFSET + i * 4;
        let rec = &data[off..off + 4];
        let station = rec[0];
        if station == 0 { break; }
        // rec[1] = td: bit 0 = PM flag; rec[2]/rec[3] = time high/low bytes
        // time is seconds within a 12-hour period
        let pm = rec[1] & 0x01 != 0;
        let time_s = ((rec[2] as u32) << 8) | rec[3] as u32;
        let time_s = time_s + if pm { 43200 } else { 0 };
        punches.push(ControlPunch { station, time_s });
    }

    Some(CardReadout { card_id, punches })
}

// ─── Port discovery ────────────────────────────────────────────────────────────

pub fn find_si_port() -> Option<String> {
    serialport::available_ports().ok()?.into_iter().find_map(|p| {
        if let serialport::SerialPortType::UsbPort(info) = p.port_type {
            if info.vid == SI_VID && info.pid == SI_PID {
                return Some(p.port_name);
            }
        }
        None
    })
}

// ─── Platform-specific hotplug ─────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub mod hotplug {
    use super::{SI_VID, SI_PID};
    use std::os::unix::io::AsRawFd;
    use std::sync::mpsc::Sender;

    #[derive(Debug)]
    pub enum HotplugEvent {
        Connected(String),
        Disconnected(String),
    }

    /// Spawns a background thread that delivers udev "tty" add/remove events
    /// for the SportIdent USB device via the provided sender.
    ///
    /// udev's `Socket::iter()` is non-blocking, so we poll(2) the fd to block
    /// efficiently between events instead of busy-looping.
    pub fn watch(tx: Sender<HotplugEvent>) -> Result<(), anyhow::Error> {
        let socket = udev::MonitorBuilder::new()?
            .match_subsystem("tty")?
            .listen()?;

        std::thread::Builder::new()
            .name("si-hotplug".into())
            .spawn(move || {
                let fd = socket.as_raw_fd();
                loop {
                    // Block up to 1 s waiting for a udev event on the netlink socket.
                    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                    let ret = unsafe { libc::poll(&mut pfd, 1, 1000) };
                    if ret <= 0 { continue; }

                    for event in socket.iter() {
                        let vid = event.property_value("ID_VENDOR_ID")
                            .and_then(|v| v.to_str())
                            .and_then(|s| u16::from_str_radix(s, 16).ok());
                        let pid = event.property_value("ID_MODEL_ID")
                            .and_then(|v| v.to_str())
                            .and_then(|s| u16::from_str_radix(s, 16).ok());

                        if vid != Some(SI_VID) || pid != Some(SI_PID) { continue; }

                        let path = match event.devnode().and_then(|p| p.to_str()) {
                            Some(p) => p.to_owned(),
                            None => continue,
                        };

                        let evt = match event.event_type() {
                            udev::EventType::Add => HotplugEvent::Connected(path),
                            udev::EventType::Remove => HotplugEvent::Disconnected(path),
                            _ => continue,
                        };

                        if tx.send(evt).is_err() { return; }
                    }
                }
            })?;

        Ok(())
    }
}

// On macOS, serialport uses IOKit internally; on Windows it uses SetupAPI.
// Polling available_ports() every 500ms surfaces their results without requiring
// us to write platform bindings ourselves.
#[cfg(not(target_os = "linux"))]
pub mod hotplug {
    use super::{SI_VID, SI_PID};
    use std::collections::HashSet;
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    #[derive(Debug)]
    pub enum HotplugEvent {
        Connected(String),
        Disconnected(String),
    }

    pub fn watch(tx: Sender<HotplugEvent>) -> Result<(), anyhow::Error> {
        std::thread::Builder::new()
            .name("si-hotplug".into())
            .spawn(move || {
                let mut known: HashSet<String> = si_ports();
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let current = si_ports();
                    for port in current.difference(&known) {
                        if tx.send(HotplugEvent::Connected(port.clone())).is_err() { return; }
                    }
                    for port in known.difference(&current) {
                        if tx.send(HotplugEvent::Disconnected(port.clone())).is_err() { return; }
                    }
                    known = current;
                }
            })?;
        Ok(())
    }

    fn si_ports() -> HashSet<String> {
        serialport::available_ports()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                if let serialport::SerialPortType::UsbPort(info) = p.port_type {
                    if info.vid == SI_VID && info.pid == SI_PID {
                        return Some(p.port_name);
                    }
                }
                None
            })
            .collect()
    }
}

// ─── Background worker ─────────────────────────────────────────────────────────

/// Spawns a background thread that watches for the SportIdent USB master,
/// opens it when connected, and forwards card readouts via the returned receiver.
pub fn spawn_si_worker() -> std::sync::mpsc::Receiver<CardReadout> {
    let (out_tx, out_rx) = std::sync::mpsc::channel::<CardReadout>();

    std::thread::Builder::new()
        .name("si-worker".into())
        .spawn(move || {
            let (hp_tx, hp_rx) = std::sync::mpsc::channel::<hotplug::HotplugEvent>();

            // Deliver an immediate Connected event if the device is already plugged in.
            if let Some(port) = find_si_port() {
                let _ = hp_tx.send(hotplug::HotplugEvent::Connected(port));
            }

            if let Err(e) = hotplug::watch(hp_tx) {
                log::error!("SI hotplug unavailable: {}", e);
                return;
            }

            loop {
                let port_path = match hp_rx.recv() {
                    Ok(hotplug::HotplugEvent::Connected(p)) => p,
                    Ok(hotplug::HotplugEvent::Disconnected(p)) => {
                        log::info!("SportIdent disconnected: {}", p);
                        continue;
                    }
                    Err(_) => break,
                };

                log::info!("SportIdent connected: {}", port_path);

                let mut reader = match SiReader::open(&port_path) {
                    Ok(r) => r,
                    Err(e) => { log::warn!("cannot open SI port {}: {}", port_path, e); continue; }
                };

                loop {
                    match reader.read_event() {
                        Ok(Some(SiEvent::CardReadout(readout))) => {
                            log::info!("SI card {} ({} punches)", readout.card_id, readout.punches.len());
                            if out_tx.send(readout).is_err() { return; }
                        }
                        Ok(Some(SiEvent::CardRemoved)) => {
                            log::debug!("SI card removed");
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("SI port error: {}", e);
                            break;
                        }
                    }

                    // A disconnect event from hotplug means the port is gone.
                    if let Ok(hotplug::HotplugEvent::Disconnected(p)) = hp_rx.try_recv() {
                        log::info!("SportIdent disconnected: {}", p);
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn SI worker");

    out_rx
}
