//! SportIdent protocol parsing, ported from `lora-server/src/sportident.rs`.
//!
//! This is a direct port of that file's transport-agnostic logic (packet
//! framing, CRC16, card-image/autosend-punch parsing) — not a rewrite. The
//! RPi side reads bytes from a `serialport::SerialPort`; here `SiReader<T>`
//! is generic over any `std::io::Read + std::io::Write` byte source, so the
//! same parsing state machine works unchanged once fed from the CP210x VCP
//! transport in `cp210x.rs` instead. If this proves out on real hardware,
//! the transport-agnostic half of both files is a good candidate for the
//! shared `sportident-proto` crate the original design doc called for —
//! not done yet since duplicating a working, tested parser was lower-risk
//! than extracting a crate around something not yet validated.

use std::io::{Read, Write};
use std::time::Duration;

// ─── USB device identity ───────────────────────────────────────────────────
// SportIdent BSM7/BSM8-USB master stations use a Silicon Labs CP210x
// USB-to-UART bridge internally — this is why cp210x.rs exists at all rather
// than the plain esp-idf usb_host_cdc_acm driver (CP210x isn't a standards-
// compliant CDC-ACM device).
pub const SI_VID: u16 = 0x10C4;
pub const SI_PID: u16 = 0x800A;
pub const SI_BAUD: u32 = 38400;

// ─── Protocol constants ─────────────────────────────────────────────────────

const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const WAKEUP: u8 = 0xFF;

const C_SI5_DET: u8 = 0xE5;
const C_SI6_DET: u8 = 0xE6;
const C_SI9_DET: u8 = 0xE8;
const C_SI_REM: u8 = 0xE7;

const C_GET_SI5: u8 = 0xB1;
const C_GET_SI6: u8 = 0xE1;
const C_GET_SI9: u8 = 0xEF;

const C_TRANS_REC: u8 = 0xD3;

const PUNCH_CARD_OFFSET: usize = 0;
const PUNCH_TIME_OFFSET: usize = 5;
const PUNCH_BACKUP_OFFSET_OFFSET: usize = 8;

const REC_PTD: usize = 0;
const REC_CN: usize = 1;
const REC_PTH: usize = 2;
const REC_PTL: usize = 3;

const SI9_PUNCH_COUNT_OFFSET: usize = 0x16;
const SI9_PUNCH_START_OFFSET: usize = 0x38;
const SI9_PUNCH_MAX: usize = 50;

const SI1011_PUNCH_COUNT_OFFSET: usize = 0x16;
const SI1011_PUNCH_START_OFFSET: usize = 128;
const SI1011_PUNCH_MAX: usize = 64;

const SI6_PUNCH_COUNT_OFFSET: usize = 18;
const SI6_PUNCH_START_OFFSET: usize = 128;
const SI6_PUNCH_MAX: usize = 64;

const SI5_PUNCH_COUNT_OFFSET: usize = 23;
const SI5_PUNCH_START_OFFSET: usize = 32;
const SI5_PUNCH_MAX: usize = 30;
const SI5_REC_LEN: usize = 3;
const SI5_REC_CN: usize = 0;
const SI5_REC_PTH: usize = 1;
const SI5_REC_PTL: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CardSeries { Si6, Si9, Si8, Si10Or11, Other(u8) }

impl CardSeries {
    fn from_byte(b: u8) -> Self {
        match b {
            0x00 => CardSeries::Si6,
            0x01 => CardSeries::Si9,
            0x02 => CardSeries::Si8,
            0x0F => CardSeries::Si10Or11,
            other => CardSeries::Other(other),
        }
    }
}

const C_GET_SYS_VAL: u8 = 0x83;
const O_MODE: u8 = 0x71;
const O_PROTO: u8 = 0x74;
const M_CONTROL: u8 = 0x02;

const C_GET_BACKUP: u8 = 0x81;
const BACKUP_REC_LEN: u8 = 8;

// ─── Public types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ControlPunch {
    pub station: u8,
    pub time_s: u32,
}

#[derive(Debug, Clone)]
pub struct CardReadout {
    pub card_id: u32,
    pub punches: Vec<ControlPunch>,
}

impl CardReadout {
    /// Wire format sent over LoRa: `PUNCH <origin> <card_id> <station>:<time_s>,...`
    /// — must match `lora-server`'s `CardReadout::to_payload`/`parse_payload`
    /// exactly, since the RPi parses this same format on receive.
    pub fn to_payload(&self, origin: u16) -> String {
        let punches: String = self.punches.iter()
            .map(|p| format!("{}:{}", p.station, p.time_s))
            .collect::<Vec<_>>()
            .join(",");
        format!("PUNCH {} {} {}", origin, self.card_id, punches)
    }
}

#[derive(Debug)]
pub enum SiEvent {
    CardReadout(CardReadout),
    CardRemoved,
}

#[derive(Debug, Clone, Copy)]
enum CardType { Si5, Si6, Si9 }

#[derive(Debug)]
enum ParsedPacket {
    CardInserted { card_type: CardType, series: u8, card_id: u32 },
    CardRemoved,
    CardData { data: Vec<u8> },
    Punch { control: u16, data: Vec<u8> },
    BackupRecord { control: u16, data: Vec<u8> },
    SysVal { data: Vec<u8> },
}

// ─── CRC-16 (SportIdent's own algorithm) — see lora-server/src/sportident.rs
// for the full derivation notes; ported verbatim. ────────────────────────────

fn crc16(data: &[u8]) -> u16 {
    if data.is_empty() { return 0; }

    let mut crc: u16 = if data.len() >= 2 {
        ((data[0] as u16) << 8) | data[1] as u16
    } else {
        (data[0] as u16) << 8
    };

    let rest = if data.len() > 2 { &data[2..] } else { &[][..] };
    if rest.is_empty() {
        return crc;
    }

    let mut padded = rest.to_vec();
    if padded.len() % 2 == 0 {
        padded.extend_from_slice(&[0, 0]);
    } else {
        padded.push(0);
    }

    for chunk in padded.chunks_exact(2) {
        let mut val: u16 = ((chunk[0] as u16) << 8) | chunk[1] as u16;
        for _ in 0..16 {
            let crc_top = crc & 0x8000 != 0;
            let val_top = val & 0x8000 != 0;
            crc <<= 1;
            if val_top { crc = crc.wrapping_add(1); }
            if crc_top { crc ^= 0x8005; }
            val <<= 1;
        }
    }

    crc
}

fn build_command(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut pkt = vec![WAKEUP, STX, cmd, data.len() as u8];
    pkt.extend_from_slice(data);
    let crc = crc16(&pkt[2..]);
    pkt.extend_from_slice(&[(crc >> 8) as u8, crc as u8]);
    pkt.push(ETX);
    pkt
}

// ─── SI card reader, generic over the byte transport ───────────────────────

pub struct SiReader<T: Read + Write> {
    port: T,
    buf: Vec<u8>,
    pending: Option<(CardType, u8, u32)>,
    next_backup_offset: Option<u32>,
    recovery_pending: bool,
}

impl<T: Read + Write> SiReader<T> {
    /// Access to the underlying transport — e.g. so a caller can check a
    /// transport-specific hotplug/disconnect signal (`sportident.rs` itself
    /// stays generic over `T` and doesn't know about any such signal).
    pub fn transport(&self) -> &T {
        &self.port
    }

    pub fn new(port: T) -> Self {
        let mut reader = Self {
            port, buf: Vec::new(), pending: None,
            next_backup_offset: None, recovery_pending: false,
        };
        reader.log_protocol_config();
        reader
    }

    fn send(&mut self, cmd: u8, data: &[u8]) -> anyhow::Result<()> {
        self.port.write_all(&build_command(cmd, data))?;
        Ok(())
    }

    fn request(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> anyhow::Result<ParsedPacket> {
        self.send(cmd, data)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut tmp = [0u8; 256];
            match self.port.read(&mut tmp) {
                Ok(n) if n > 0 => self.buf.extend_from_slice(&tmp[..n]),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if let Some(pkt) = self.try_parse_packet() {
                return Ok(pkt);
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("no response from SI station");
            }
        }
    }

    fn query_sys_val(&mut self, addr: u8) -> anyhow::Result<u8> {
        match self.request(C_GET_SYS_VAL, &[addr, 0x01], Duration::from_millis(500))? {
            ParsedPacket::SysVal { data } if data.len() >= 2 => Ok(data[1]),
            other => anyhow::bail!("unexpected response to sys-val query: {:?}", other),
        }
    }

    fn log_protocol_config(&mut self) {
        let proto = match self.query_sys_val(O_PROTO) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("could not query SI station protocol config: {}", e);
                return;
            }
        };
        let ext_proto = proto & 0x01 != 0;
        let auto_send = proto & 0x02 != 0;
        let mode = self.query_sys_val(O_MODE).ok();

        match mode {
            Some(m) => log::info!(
                "SI station config: ext_proto={} auto_send={} mode=0x{:02X}",
                ext_proto, auto_send, m
            ),
            None => log::info!(
                "SI station config: ext_proto={} auto_send={} mode=<query failed>",
                ext_proto, auto_send
            ),
        }

        if mode == Some(M_CONTROL) && !(ext_proto && auto_send) {
            log::warn!(
                "SI station is in Control mode but {}{} — punches will not be sent. \
                 Enable both in SI Config+ (separate from the operating-mode setting).",
                if ext_proto { "" } else { "Extended Protocol is off " },
                if auto_send { "" } else { "Autosend is off" },
            );
        }
    }

    fn try_parse_packet(&mut self) -> Option<ParsedPacket> {
        loop {
            let stx = self.buf.iter().position(|&b| b == STX)?;
            self.buf.drain(..stx);

            if self.buf.len() < 3 { return None; }

            let cmd = self.buf[1];
            let len = self.buf[2] as usize;
            let total = 1 + 1 + 1 + len + 2 + 1;

            if self.buf.len() < total { return None; }
            if len < 2 {
                self.buf.remove(0);
                continue;
            }

            if self.buf[total - 1] != ETX {
                self.buf.remove(0);
                continue;
            }

            let crc_received = ((self.buf[3 + len] as u16) << 8) | self.buf[3 + len + 1] as u16;
            let crc_computed = crc16(&self.buf[1..3 + len]);
            if crc_received != crc_computed {
                log::warn!(
                    "SI packet CRC mismatch (cmd 0x{:02X}): got {:04X}, expected {:04X} — processing anyway",
                    cmd, crc_received, crc_computed
                );
            }

            let station = ((self.buf[3] as u16) << 8) | self.buf[4] as u16;
            let data = self.buf[5..3 + len].to_vec();
            self.buf.drain(..total);

            match cmd {
                C_TRANS_REC => return Some(ParsedPacket::Punch { control: station, data }),
                C_SI5_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si5, series: 0, card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI6_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si6, series: data[0], card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI9_DET if data.len() >= 4 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si9, series: data[0], card_id: card_id_3b(&data[1..4]),
                    });
                }
                C_SI_REM => return Some(ParsedPacket::CardRemoved),
                C_GET_SI5 | C_GET_SI6 | C_GET_SI9 => return Some(ParsedPacket::CardData { data }),
                C_GET_SYS_VAL => return Some(ParsedPacket::SysVal { data }),
                C_GET_BACKUP => return Some(ParsedPacket::BackupRecord { control: station, data }),
                _ => continue,
            }
        }
    }

    /// Read bytes from the transport and return the next complete SI event,
    /// if any. Returns `Ok(None)` on timeout/no-data — call in a loop.
    pub fn read_event(&mut self) -> anyhow::Result<Option<SiEvent>> {
        let mut tmp = [0u8; 256];
        match self.port.read(&mut tmp) {
            Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        while let Some(pkt) = self.try_parse_packet() {
            match pkt {
                ParsedPacket::CardInserted { card_type, series, card_id } => {
                    self.pending = Some((card_type, series, card_id));
                    let get_cmd = match card_type {
                        CardType::Si5 => C_GET_SI5,
                        CardType::Si6 => C_GET_SI6,
                        CardType::Si9 => C_GET_SI9,
                    };
                    self.send(get_cmd, &[])?;
                }
                ParsedPacket::CardData { data } => {
                    if let Some((card_type, series, card_id)) = self.pending.take() {
                        if let Some(readout) = parse_card_data(card_type, series, card_id, &data) {
                            return Ok(Some(SiEvent::CardReadout(readout)));
                        }
                    }
                }
                ParsedPacket::CardRemoved => {
                    self.pending = None;
                    return Ok(Some(SiEvent::CardRemoved));
                }
                ParsedPacket::Punch { control, data } => {
                    self.track_backup_offset(&data);
                    if let Some(readout) = parse_punch(control, &data, now_seconds_of_day()) {
                        return Ok(Some(SiEvent::CardReadout(readout)));
                    }
                }
                ParsedPacket::BackupRecord { control, data } => {
                    self.recovery_pending = false;
                    if let Some(off) = self.next_backup_offset {
                        self.next_backup_offset = Some(off + BACKUP_REC_LEN as u32);
                    }
                    if let Some(readout) = parse_backup_record(control, &data, now_seconds_of_day()) {
                        return Ok(Some(SiEvent::CardReadout(readout)));
                    }
                }
                ParsedPacket::SysVal { .. } => {}
            }
        }

        Ok(None)
    }

    fn track_backup_offset(&mut self, data: &[u8]) {
        if data.len() < PUNCH_BACKUP_OFFSET_OFFSET + 3 { return; }
        let cur_offset = ((data[PUNCH_BACKUP_OFFSET_OFFSET] as u32) << 16)
            | ((data[PUNCH_BACKUP_OFFSET_OFFSET + 1] as u32) << 8)
            | data[PUNCH_BACKUP_OFFSET_OFFSET + 2] as u32;

        match self.next_backup_offset {
            Some(expected) if cur_offset > expected => {
                if !self.recovery_pending {
                    log::warn!(
                        "SI: gap in punch stream detected, recovering from backup memory (offset {} -> {})",
                        expected, cur_offset
                    );
                    let off = expected.to_be_bytes();
                    if self.send(C_GET_BACKUP, &[off[1], off[2], off[3], BACKUP_REC_LEN]).is_ok() {
                        self.recovery_pending = true;
                    }
                }
            }
            _ => self.next_backup_offset = Some(cur_offset + BACKUP_REC_LEN as u32),
        }
    }
}

// ─── Card data parsing (verbatim port) ──────────────────────────────────────

fn card_id_3b(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

fn parse_card_data(card_type: CardType, series: u8, card_id: u32, data: &[u8]) -> Option<CardReadout> {
    match card_type {
        CardType::Si9 => match CardSeries::from_byte(series) {
            CardSeries::Si10Or11 => parse_4byte_punches(
                card_id, data, SI1011_PUNCH_COUNT_OFFSET, SI1011_PUNCH_START_OFFSET, SI1011_PUNCH_MAX,
            ),
            _ => parse_4byte_punches(
                card_id, data, SI9_PUNCH_COUNT_OFFSET, SI9_PUNCH_START_OFFSET, SI9_PUNCH_MAX,
            ),
        },
        CardType::Si6 => parse_4byte_punches(
            card_id, data, SI6_PUNCH_COUNT_OFFSET, SI6_PUNCH_START_OFFSET, SI6_PUNCH_MAX,
        ),
        CardType::Si5 => parse_si5(card_id, data),
    }
}

fn parse_4byte_punches(
    card_id: u32, data: &[u8], count_offset: usize, start_offset: usize, punch_max: usize,
) -> Option<CardReadout> {
    if data.len() <= start_offset { return None; }

    let punch_count = (*data.get(count_offset)? as usize).min(punch_max);
    let max = (data.len() - start_offset) / 4;
    let count = punch_count.min(max);

    let mut punches = Vec::with_capacity(count);
    for i in 0..count {
        let off = start_offset + i * 4;
        let rec = &data[off..off + 4];
        let station = rec[REC_CN];
        if station == 0 { break; }
        let pm = rec[REC_PTD] & 0x01 != 0;
        let time_s = ((rec[REC_PTH] as u32) << 8) | rec[REC_PTL] as u32;
        let time_s = time_s + if pm { 43_200 } else { 0 };
        punches.push(ControlPunch { station, time_s });
    }

    Some(CardReadout { card_id, punches })
}

fn parse_si5(card_id: u32, data: &[u8]) -> Option<CardReadout> {
    if data.len() <= SI5_PUNCH_START_OFFSET { return None; }

    let punch_count = (*data.get(SI5_PUNCH_COUNT_OFFSET)? as usize)
        .saturating_sub(1)
        .min(SI5_PUNCH_MAX);

    let mut punches = Vec::with_capacity(punch_count);
    let mut i = SI5_PUNCH_START_OFFSET;
    for _ in 0..punch_count {
        if (i - SI5_PUNCH_START_OFFSET).is_multiple_of(16) {
            i += 1;
        }
        if i + SI5_REC_LEN > data.len() { break; }
        let rec = &data[i..i + SI5_REC_LEN];
        let station = rec[SI5_REC_CN];
        let time_s = ((rec[SI5_REC_PTH] as u32) << 8) | rec[SI5_REC_PTL] as u32;
        punches.push(ControlPunch { station, time_s });
        i += SI5_REC_LEN;
    }

    Some(CardReadout { card_id, punches })
}

fn parse_punch(control: u16, data: &[u8], now_s: u32) -> Option<CardReadout> {
    if data.len() < PUNCH_TIME_OFFSET + 2 { return None; }

    let card_id = card_id_3b(&data[PUNCH_CARD_OFFSET + 1..PUNCH_CARD_OFFSET + 4]);

    let raw_time = ((data[PUNCH_TIME_OFFSET] as u32) << 8) | data[PUNCH_TIME_OFFSET + 1] as u32;
    if raw_time >= 86_400 { return None; }
    let time_s = resolve_autosend_time(raw_time % 43_200, now_s);

    let station = (control & 0xFF) as u8;
    Some(CardReadout { card_id, punches: vec![ControlPunch { station, time_s }] })
}

fn parse_backup_record(control: u16, data: &[u8], now_s: u32) -> Option<CardReadout> {
    const ECHO_LEN: usize = 3;
    if data.len() < ECHO_LEN + PUNCH_TIME_OFFSET + 2 { return None; }
    let record = &data[ECHO_LEN..];

    let card_id = card_id_3b(&record[0..3]);
    let raw_time = ((record[PUNCH_TIME_OFFSET] as u32) << 8) | record[PUNCH_TIME_OFFSET + 1] as u32;
    if raw_time >= 86_400 { return None; }
    let time_s = resolve_autosend_time(raw_time % 43_200, now_s);

    let station = (control & 0xFF) as u8;
    Some(CardReadout { card_id, punches: vec![ControlPunch { station, time_s }] })
}

fn resolve_autosend_time(raw_time_s: u32, now_s: u32) -> u32 {
    const DAY: u32 = 86_400;
    const NOON: u32 = 43_200;
    let ref_s = (now_s + 2 * 3600) % DAY;

    if ref_s < NOON {
        if raw_time_s < ref_s { raw_time_s } else { raw_time_s + NOON }
    } else if raw_time_s < ref_s - NOON {
        raw_time_s + NOON
    } else {
        raw_time_s
    }
}

/// Current local wall-clock time as seconds since midnight. NOTE: without an
/// SNTP client running (not set up in this firmware yet), the ESP32's clock
/// is whatever it booted with — this can misresolve the AM/PM ambiguity in
/// `resolve_autosend_time` until NTP sync is added. Not a blocker for
/// building/testing the USB link itself, but a known follow-up.
fn now_seconds_of_day() -> u32 {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u32) * 3600 + (tm.tm_min as u32) * 60 + tm.tm_sec as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc16_two_bytes_is_just_the_seed() {
        assert_eq!(crc16(&[0xAB, 0xCD]), 0xABCD);
    }

    #[test]
    fn test_parse_punch_decodes_card_and_control() {
        let data = [0x01, 0x0F, 0x42, 0x40, 0x00, 0x70, 0x80];
        let now_s = 8 * 3600 + 5;
        let readout = parse_punch(33, &data, now_s).unwrap();

        assert_eq!(readout.card_id, 0x0F4240);
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 33);
        assert_eq!(readout.punches[0].time_s, 8 * 3600);
    }

    #[test]
    fn test_to_payload_matches_lora_server_wire_format() {
        let readout = CardReadout {
            card_id: 0x0F4240,
            punches: vec![ControlPunch { station: 33, time_s: 36070 }],
        };
        assert_eq!(readout.to_payload(10), "PUNCH 10 1000000 33:36070");
    }
}
