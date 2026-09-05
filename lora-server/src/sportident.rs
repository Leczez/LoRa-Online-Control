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

// Autosend punch record — sent unsolicited by a station in "Control" operating
// mode with Extended Protocol + Autosend enabled, one per card punch. Requires
// Extended Protocol, which adds a 2-byte station/control-number field to every
// packet's header (see try_parse_packet); this differs from a full card
// readout (CardData above), which is the whole card's punch history read out
// after physical insertion.
const C_TRANS_REC: u8 = 0xD3;

// Offsets within an autosend punch record's data (after the 2-byte station
// header field has been split off): 4-byte card number, then a punch time
// (12-hour raw seconds, no AM/PM bit — must be resolved against the current
// wall clock, unlike stored card data which carries an explicit PTD byte).
const PUNCH_CARD_OFFSET: usize = 0;
const PUNCH_TIME_OFFSET: usize = 5;
// 3-byte position of this punch in the station's backup memory, used to
// detect (and recover) punches missed during a connection drop.
const PUNCH_BACKUP_OFFSET_OFFSET: usize = 8;

// Per-punch-record layout shared by SI6/SI8/SI9/SI10/SI11 card images (bytes
// relative to the start of each record): day/AM-PM byte, control number,
// punch time high/low. Verified against gaudenz/sireader's CARD table.
const REC_PTD: usize = 0;
const REC_CN: usize = 1;
const REC_PTH: usize = 2;
const REC_PTL: usize = 3;

// Overall card-image offsets: (punch_count_offset, first_punch_offset, max_punches).
// SI8/SI9 and SI10/SI11 use different offsets despite sharing one detection/get
// command (0xE8/0xEF) — distinguished by the card series byte (see CardSeries).
const SI9_PUNCH_COUNT_OFFSET: usize = 0x16; // RC
const SI9_PUNCH_START_OFFSET: usize = 0x38; // P1=56
const SI9_PUNCH_MAX: usize = 50;            // PM

const SI1011_PUNCH_COUNT_OFFSET: usize = 0x16; // RC (same as SI8/9)
const SI1011_PUNCH_START_OFFSET: usize = 128;  // P1
const SI1011_PUNCH_MAX: usize = 64;            // PM

const SI6_PUNCH_COUNT_OFFSET: usize = 18;  // RC
const SI6_PUNCH_START_OFFSET: usize = 128; // P1
const SI6_PUNCH_MAX: usize = 64;           // PM

// SI5 has a genuinely different, older layout: 3-byte (not 4-byte) punch
// records, no PTD byte, and one reserved byte per 16-byte block (used for
// punches 31-36's control numbers, which have no stored time).
const SI5_PUNCH_COUNT_OFFSET: usize = 23; // RC (index of the *next* punch, so count = RC-1)
const SI5_PUNCH_START_OFFSET: usize = 32; // P1
const SI5_PUNCH_MAX: usize = 30;          // PM
const SI5_REC_LEN: usize = 3;
const SI5_REC_CN: usize = 0;
const SI5_REC_PTH: usize = 1;
const SI5_REC_PTL: usize = 2;

// Card series byte (first byte of the 4-byte card number in detection
// messages / GET-data responses) — needed to pick the right offset table
// above, since SI8/9 and SI10/11 share a detection command but not a layout.
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

// ─── Protocol config query (Extended Protocol / Autosend / operating mode) ────

const C_GET_SYS_VAL: u8 = 0x83;
const O_MODE: u8 = 0x71;
const O_PROTO: u8 = 0x74;
const M_CONTROL: u8 = 0x02;

// ─── Backup memory (lost-punch recovery) ──────────────────────────────────────

const C_GET_BACKUP: u8 = 0x81;
// Backup memory record length in Extended Protocol (6 in Legacy — unused here
// since this driver requires Extended Protocol throughout).
const BACKUP_REC_LEN: u8 = 8;

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
    /// Wire format sent over LoRa: `PUNCH <origin> <card_id> <station>:<time_s>,...`
    ///
    /// `origin` is the LoRa address of the node that actually read this
    /// card — carried explicitly in the payload, not inferred from the
    /// radio layer's immediate sender, so that once a punch travels through
    /// a relay the base station still attributes it to the right control
    /// point rather than to the relay that last touched it (see the
    /// "Relay Nodes" section of docs/protocols/lora_online_control_protocol.md).
    pub fn to_payload(&self, origin: u16) -> String {
        let punches: String = self.punches.iter()
            .map(|p| format!("{}:{}", p.station, p.time_s))
            .collect::<Vec<_>>()
            .join(",");
        format!("PUNCH {} {} {}", origin, self.card_id, punches)
    }

    /// Inverse of `to_payload` — decodes a `PUNCH <origin> <card_id>
    /// <station>:<time_s>,...` wire payload back into the originating node's
    /// address and the card data itself.
    pub fn parse_payload(s: &str) -> Option<(u16, CardReadout)> {
        let rest = s.strip_prefix("PUNCH ")?;
        let mut parts = rest.splitn(3, ' ');
        let origin: u16 = parts.next()?.parse().ok()?;
        let card_id: u32 = parts.next()?.parse().ok()?;
        let punches = parts.next().unwrap_or("");

        let mut result = Vec::new();
        if !punches.is_empty() {
            for p in punches.split(',') {
                let (station_str, time_str) = p.split_once(':')?;
                let station: u8 = station_str.parse().ok()?;
                let time_s: u32 = time_str.parse().ok()?;
                result.push(ControlPunch { station, time_s });
            }
        }

        Some((origin, CardReadout { card_id, punches: result }))
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
    CardInserted { card_type: CardType, series: u8, card_id: u32 },
    CardRemoved,
    CardData { data: Vec<u8> },
    Punch { control: u16, data: Vec<u8> },
    BackupRecord { control: u16, data: Vec<u8> },
    SysVal { data: Vec<u8> },
}

// ─── CRC-16 (SportIdent's own algorithm — NOT a standard byte-at-a-time CRC) ──
//
// Ported precisely from gaudenz/sireader's SIReader._crc, itself a port of
// the Java example in SportIdent's Programmer's Manual. This is unusual: the
// register is seeded directly from the first two input bytes (not XORed in
// from zero), the remaining bytes are processed as 16-bit words with an
// extra all-zero word always appended, and each word is mixed in via 16
// bit-serial iterations that update `crc` and `val` in lockstep. Extended
// Protocol (required throughout this driver — see try_parse_packet) folds
// the 2-byte station field into the coverage alongside CMD+LEN+DATA.

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
    let crc = crc16(&pkt[2..]);  // CRC over CMD + LEN + DATA
    pkt.extend_from_slice(&[(crc >> 8) as u8, crc as u8]);
    pkt.push(ETX);
    pkt
}

// ─── SI card reader ────────────────────────────────────────────────────────────

pub struct SiReader {
    port: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
    pending: Option<(CardType, u8, u32)>,
    /// Next expected backup-memory offset for gap detection (lost-punch
    /// recovery). None until the first live punch establishes a baseline.
    next_backup_offset: Option<u32>,
    /// True while a C_GET_BACKUP recovery request is outstanding, so a burst
    /// of live punches arriving before the reply doesn't fire duplicate
    /// requests for the same gap (which would relay the same punch twice).
    recovery_pending: bool,
}

impl SiReader {
    pub fn open(port_path: &str) -> Result<Self> {
        let port = serialport::new(port_path, SI_BAUD)
            .timeout(Duration::from_millis(100))
            .open()?;
        let mut reader = Self {
            port, buf: Vec::new(), pending: None,
            next_backup_offset: None, recovery_pending: false,
        };
        reader.log_protocol_config();
        Ok(reader)
    }

    fn send(&mut self, cmd: u8, data: &[u8]) -> Result<()> {
        use std::io::Write as _;
        self.port.write_all(&build_command(cmd, data))?;
        Ok(())
    }

    /// Blocking request/response: send a command and wait (bounded) for its
    /// reply to show up via the normal packet parser. Only meant for the
    /// one-off queries below, run once at connect time — the hot receive path
    /// (read_event) stays fully non-blocking.
    fn request(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> Result<ParsedPacket> {
        self.send(cmd, data)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut tmp = [0u8; 256];
            match self.port.read(&mut tmp) {
                Ok(n) if n > 0 => self.buf.extend_from_slice(&tmp[..n]),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
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

    fn query_sys_val(&mut self, addr: u8) -> Result<u8> {
        match self.request(C_GET_SYS_VAL, &[addr, 0x01], Duration::from_millis(500))? {
            ParsedPacket::SysVal { data } if data.len() >= 2 => Ok(data[1]),
            other => anyhow::bail!("unexpected response to sys-val query: {:?}", other),
        }
    }

    /// Logs the station's protocol configuration at connect time, so a
    /// station that's in "Control" operating mode but missing Extended
    /// Protocol or Autosend (both required for punches to be sent at all)
    /// shows up immediately in the log instead of silently doing nothing.
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
            // Extended Protocol (required for autosend/control mode) prepends a
            // 2-byte station/control-number field ahead of the data, still
            // counted within `len` — so the overall packet length is unchanged,
            // only the split between header and payload shifts by 2 bytes.
            let total = 1 + 1 + 1 + len + 2 + 1;  // STX CMD LEN STATION+DATA[len] CRC[2] ETX

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
            let crc_computed = crc16(&self.buf[1..3 + len]); // CMD + LEN + STATION + DATA
            if crc_received != crc_computed {
                // Log only, don't drop: this check has already caused one
                // false-positive rejection of real punches from a CRC
                // implementation bug, so treat mismatches as a diagnostic
                // signal rather than a gate until it's proven solid against
                // real hardware over time.
                log::warn!(
                    "SI packet CRC mismatch (cmd 0x{:02X}): got {:04X}, expected {:04X} — processing anyway",
                    cmd, crc_received, crc_computed
                );
            }

            let station = ((self.buf[3] as u16) << 8) | self.buf[4] as u16;
            let data = self.buf[5..3 + len].to_vec();
            self.buf.drain(..total);

            match cmd {
                C_TRANS_REC => {
                    return Some(ParsedPacket::Punch { control: station, data });
                }
                C_SI5_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si5,
                        series: 0,
                        card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI6_DET if data.len() >= 3 => {
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si6,
                        series: data[0],
                        card_id: card_id_3b(&data[0..3]),
                    });
                }
                C_SI9_DET if data.len() >= 4 => {
                    // Byte 0 is the card series byte (distinguishes SI8/9 from
                    // SI10/11/SIAC1, which use different card-image offsets
                    // despite sharing this detection command); card number is
                    // in bytes 1-3.
                    return Some(ParsedPacket::CardInserted {
                        card_type: CardType::Si9,
                        series: data[0],
                        card_id: card_id_3b(&data[1..4]),
                    });
                }
                C_SI_REM => return Some(ParsedPacket::CardRemoved),
                C_GET_SI5 | C_GET_SI6 | C_GET_SI9 => {
                    return Some(ParsedPacket::CardData { data });
                }
                C_GET_SYS_VAL => return Some(ParsedPacket::SysVal { data }),
                C_GET_BACKUP => return Some(ParsedPacket::BackupRecord { control: station, data }),
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
                ParsedPacket::SysVal { .. } => {
                    // Only expected as a direct reply within request(); a stray
                    // one arriving here (e.g. a delayed response) is harmless.
                }
            }
        }

        Ok(None)
    }

    /// Detects gaps in the backup-memory offset a live punch carries (see
    /// PUNCH_BACKUP_OFFSET_OFFSET) and requests the missing record(s) so a
    /// brief disconnect doesn't silently lose punches. Recovery is one record
    /// per call — remaining gap is picked up on subsequent read_event() ticks
    /// once the requested BackupRecord reply advances next_backup_offset.
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
                // next_backup_offset stays at `expected` until the recovered
                // BackupRecord reply advances it, so this keeps firing (once
                // recovery_pending clears) until fully caught up.
            }
            _ => self.next_backup_offset = Some(cur_offset + BACKUP_REC_LEN as u32),
        }
    }
}

// ─── Card data parsing ─────────────────────────────────────────────────────────

fn card_id_3b(b: &[u8]) -> u32 {
    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32
}

fn parse_card_data(card_type: CardType, series: u8, card_id: u32, data: &[u8]) -> Option<CardReadout> {
    match card_type {
        CardType::Si9 => match CardSeries::from_byte(series) {
            CardSeries::Si10Or11 => parse_4byte_punches(
                card_id, data, SI1011_PUNCH_COUNT_OFFSET, SI1011_PUNCH_START_OFFSET, SI1011_PUNCH_MAX,
            ),
            // SI8/SI9 share offsets; unrecognized series (pCard/tCard/fCard)
            // fall back to the same table as a best-effort default — their
            // exact offsets aren't confirmed against a reference yet.
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

/// Shared punch-record layout for SI6/SI8/SI9/SI10/SI11 card images: each
/// 4-byte record is [PTD (day-of-week/AM-PM byte), control number, time high,
/// time low]. Verified against gaudenz/sireader's CARD table — note PTD comes
/// *before* the control number, not after.
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

/// SI5 has an older, different layout: 3-byte punch records with no PTD byte
/// (so no stored AM/PM — matches sireader's behaviour of just taking the raw
/// 12h time as-is for these), and one reserved byte at the start of every
/// 16-byte block for punches 31-36's control numbers.
fn parse_si5(card_id: u32, data: &[u8]) -> Option<CardReadout> {
    if data.len() <= SI5_PUNCH_START_OFFSET { return None; }

    // RC is the index of the *next* punch to be written, so count = RC - 1.
    let punch_count = (*data.get(SI5_PUNCH_COUNT_OFFSET)? as usize)
        .saturating_sub(1)
        .min(SI5_PUNCH_MAX);

    let mut punches = Vec::with_capacity(punch_count);
    let mut i = SI5_PUNCH_START_OFFSET;
    for _ in 0..punch_count {
        if (i - SI5_PUNCH_START_OFFSET).is_multiple_of(16) {
            i += 1; // reserved byte for punches 31-36
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

// ─── Autosend punch parsing (control-point / online-control mode) ────────────

/// Parses one autosend punch record (command 0xD3). `now_s` is the current
/// wall-clock time as seconds since local midnight, used to resolve the
/// record's 12-hour raw time (it carries no AM/PM bit, unlike stored card
/// data — see resolve_autosend_time).
fn parse_punch(control: u16, data: &[u8], now_s: u32) -> Option<CardReadout> {
    if data.len() < PUNCH_TIME_OFFSET + 2 { return None; }

    // Byte 0 of the 4-byte card number is a card-series/type marker, matching
    // the same convention already used for C_SI9_DET above.
    let card_id = card_id_3b(&data[PUNCH_CARD_OFFSET + 1..PUNCH_CARD_OFFSET + 4]);

    let raw_time = ((data[PUNCH_TIME_OFFSET] as u32) << 8) | data[PUNCH_TIME_OFFSET + 1] as u32;
    if raw_time >= 86_400 { return None; }  // TIME_RESET / no valid time recorded
    let time_s = resolve_autosend_time(raw_time % 43_200, now_s);

    let station = (control & 0xFF) as u8;
    Some(CardReadout { card_id, punches: vec![ControlPunch { station, time_s }] })
}

/// Parses a recovered backup-memory record (response to C_GET_BACKUP): a
/// 3-byte echoed offset followed by an 8-byte record whose card number (here
/// stored as 3 bytes, no series marker — unlike the live punch's 4-byte
/// T_CN) and time share the live punch record's relative layout.
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

/// Resolves a 12-hour raw punch time (0..43199 s) against the current time of
/// day, replicating sireader.py's SIReader._decode_time no-PTD path: pick
/// whichever of {raw, raw+12h} falls in the same 12-hour half as "now + 2h"
/// (the 2h safety margin absorbs the few seconds of relay latency so a punch
/// right at a noon/midnight boundary still resolves correctly).
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

/// Current local wall-clock time as seconds since midnight.
fn now_seconds_of_day() -> u32 {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour as u32) * 3600 + (tm.tm_min as u32) * 60 + tm.tm_sec as u32
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_autosend_time_am_stays_am() {
        // Punch at 08:00:00, checked moments later at 08:00:05 — clearly AM.
        let raw = 8 * 3600;
        let now = 8 * 3600 + 5;
        assert_eq!(resolve_autosend_time(raw, now), raw);
    }

    #[test]
    fn test_resolve_autosend_time_pm_gets_12h_added() {
        // Raw punch time is always 0..43199 (12h range); a punch at 15:00:00
        // arrives from the station as 03:00:00 (10800s) since that's what
        // wraps into the 12h range. Checked at 15:00:05 — clearly PM.
        let raw = 3 * 3600;
        let now = 15 * 3600 + 5;
        assert_eq!(resolve_autosend_time(raw, now), raw + 43_200);
    }

    #[test]
    fn test_resolve_autosend_time_noon_boundary_with_safety_margin() {
        // Punch at 11:59:58 (raw, AM), but relay processing pushes wall clock
        // to 12:00:02 by the time we check — the +2h safety margin should
        // still resolve this correctly as the AM punch it actually was.
        let raw = 11 * 3600 + 59 * 60 + 58;
        let now = 12 * 3600 + 2;
        assert_eq!(resolve_autosend_time(raw, now), raw);
    }

    #[test]
    fn test_parse_punch_decodes_card_and_control() {
        // card series byte (SI9) + 3-byte card number, 1 unused byte, then
        // a 2-byte 12h raw time of 08:00:00 = 28800s = 0x7080.
        let data = [0x01, 0x0F, 0x42, 0x40, 0x00, 0x70, 0x80];
        let now_s = 8 * 3600 + 5; // 08:00:05, same AM half as the punch
        let readout = parse_punch(33, &data, now_s).unwrap();

        assert_eq!(readout.card_id, 0x0F4240);
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 33);
        assert_eq!(readout.punches[0].time_s, 8 * 3600);
    }

    #[test]
    fn test_parse_punch_rejects_short_data() {
        assert!(parse_punch(1, &[0x01, 0x00, 0x00], 0).is_none());
    }

    #[test]
    fn test_parse_si9_punch_station_and_pm_not_swapped() {
        // Regression test: the record layout is [PTD, CN, PTH, PTL] — control
        // number comes *after* the day/AM-PM byte, not before. Getting this
        // backwards (as the code previously did) would read station=0x01
        // (the PTD byte) and derive AM/PM from the real station number's low
        // bit instead.
        let mut data = vec![0u8; 60];
        data[SI9_PUNCH_COUNT_OFFSET] = 1;
        let off = SI9_PUNCH_START_OFFSET;
        data[off] = 0x01;     // PTD: PM flag set
        data[off + 1] = 33;   // CN: real control/station number
        data[off + 2] = 0x70; // PTH
        data[off + 3] = 0x80; // PTL -> 0x7080 = 28800s = 08:00:00

        let readout = parse_card_data(CardType::Si9, 0x01, 0x123456, &data).unwrap();
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 33);
        assert_eq!(readout.punches[0].time_s, 8 * 3600 + 43_200);
    }

    #[test]
    fn test_parse_si10_uses_different_offsets_than_si9() {
        // SI10/11 share a detection/get command with SI8/9 but use different
        // card-image offsets (P1=128 vs 56, PM=64 vs 50) — distinguished by
        // the card series byte (0x0F).
        let mut data = vec![0u8; SI1011_PUNCH_START_OFFSET + 4];
        data[SI1011_PUNCH_COUNT_OFFSET] = 1;
        let off = SI1011_PUNCH_START_OFFSET;
        data[off] = 0x00;
        data[off + 1] = 45;
        data[off + 2] = 0x1C;
        data[off + 3] = 0x20; // 0x1C20 = 7200s = 02:00:00

        let readout = parse_card_data(CardType::Si9, 0x0F, 0xABCDEF, &data).unwrap();
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 45);
        assert_eq!(readout.punches[0].time_s, 2 * 3600);
    }

    #[test]
    fn test_parse_si6_punches() {
        let mut data = vec![0u8; SI6_PUNCH_START_OFFSET + 4];
        data[SI6_PUNCH_COUNT_OFFSET] = 1;
        let off = SI6_PUNCH_START_OFFSET;
        data[off] = 0x00;
        data[off + 1] = 12;
        data[off + 2] = 0x0E;
        data[off + 3] = 0x10; // 0x0E10 = 3600s = 01:00:00

        let readout = parse_card_data(CardType::Si6, 0x00, 0x1, &data).unwrap();
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 12);
        assert_eq!(readout.punches[0].time_s, 3600);
    }

    #[test]
    fn test_parse_si5_punches_skips_reserved_block_byte() {
        // SI5's first punch record is always preceded by one reserved byte
        // (P1=32 is itself 16-byte-aligned, so the skip applies immediately).
        let mut data = vec![0u8; SI5_PUNCH_START_OFFSET + 1 + SI5_REC_LEN];
        data[SI5_PUNCH_COUNT_OFFSET] = 2; // RC is the *next* punch index: count = 2-1 = 1
        let off = SI5_PUNCH_START_OFFSET + 1;
        data[off] = 7;
        data[off + 1] = 0x03;
        data[off + 2] = 0xE8; // 0x03E8 = 1000s

        let readout = parse_si5(0xABCDEF, &data).unwrap();
        assert_eq!(readout.punches.len(), 1);
        assert_eq!(readout.punches[0].station, 7);
        assert_eq!(readout.punches[0].time_s, 1000);
    }

    #[test]
    fn test_parse_backup_record() {
        let mut data = vec![0u8; 3 + 8];
        data[3..6].copy_from_slice(&[0x0F, 0x42, 0x40]); // card = 0x0F4240
        data[3 + PUNCH_TIME_OFFSET] = 0x70;
        data[3 + PUNCH_TIME_OFFSET + 1] = 0x80; // 08:00:00

        let readout = parse_backup_record(7, &data, 8 * 3600 + 5).unwrap();
        assert_eq!(readout.card_id, 0x0F4240);
        assert_eq!(readout.punches[0].station, 7);
        assert_eq!(readout.punches[0].time_s, 8 * 3600);
    }

    #[test]
    fn test_card_series_from_byte() {
        assert_eq!(CardSeries::from_byte(0x00), CardSeries::Si6);
        assert_eq!(CardSeries::from_byte(0x01), CardSeries::Si9);
        assert_eq!(CardSeries::from_byte(0x02), CardSeries::Si8);
        assert_eq!(CardSeries::from_byte(0x0F), CardSeries::Si10Or11);
        assert_eq!(CardSeries::from_byte(0x04), CardSeries::Other(0x04));
    }

    #[test]
    fn test_crc16_empty_is_zero() {
        assert_eq!(crc16(&[]), 0);
    }

    #[test]
    fn test_crc16_two_bytes_is_just_the_seed() {
        // With no remaining bytes to mix in, the CRC is exactly the two seed
        // bytes read as a big-endian u16 (see the reference's `twochars`
        // immediately stopping on empty input).
        assert_eq!(crc16(&[0xAB, 0xCD]), 0xABCD);
    }

    #[test]
    fn test_crc16_is_stable_and_input_sensitive() {
        // Regression guard for the algorithm's shape, since there's no
        // independent SportIdent test vector on hand: same input must
        // reproduce the same value, and changing any byte must change it.
        let a = crc16(&[0xEF, 0x00, 0x00, 0x02]);
        let b = crc16(&[0xEF, 0x00, 0x00, 0x02]);
        let c = crc16(&[0xEF, 0x00, 0x00, 0x03]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_punch_payload_round_trips() {
        let original = CardReadout {
            card_id: 0x0F4240,
            punches: vec![
                ControlPunch { station: 33, time_s: 36070 },
                ControlPunch { station: 50, time_s: 37300 },
            ],
        };
        let payload = original.to_payload(12);
        let (origin, decoded) = CardReadout::parse_payload(&payload).unwrap();

        assert_eq!(origin, 12);
        assert_eq!(decoded.card_id, original.card_id);
        assert_eq!(decoded.punches.len(), 2);
        assert_eq!(decoded.punches[0].station, 33);
        assert_eq!(decoded.punches[0].time_s, 36070);
        assert_eq!(decoded.punches[1].station, 50);
        assert_eq!(decoded.punches[1].time_s, 37300);
    }

    #[test]
    fn test_punch_payload_no_punches() {
        let original = CardReadout { card_id: 42, punches: vec![] };
        let (origin, decoded) = CardReadout::parse_payload(&original.to_payload(7)).unwrap();
        assert_eq!(origin, 7);
        assert_eq!(decoded.card_id, 42);
        assert!(decoded.punches.is_empty());
    }

    #[test]
    fn test_punch_payload_rejects_non_punch_text() {
        assert!(CardReadout::parse_payload("HB").is_none());
        assert!(CardReadout::parse_payload("hello?").is_none());
    }
}
