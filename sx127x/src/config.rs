// sx127x/src/config.rs

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bandwidth {
    Khz7_8,
    Khz10_4,
    Khz15_6,
    Khz20_8,
    Khz31_25,
    Khz41_7,
    Khz62_5,
    Khz125,
    Khz250,
    Khz500,
}

impl Bandwidth {
    pub fn register_value(self) -> u8 {
        match self {
            Bandwidth::Khz7_8 => 0x00,
            Bandwidth::Khz10_4 => 0x01,
            Bandwidth::Khz15_6 => 0x02,
            Bandwidth::Khz20_8 => 0x03,
            Bandwidth::Khz31_25 => 0x04,
            Bandwidth::Khz41_7 => 0x05,
            Bandwidth::Khz62_5 => 0x06,
            Bandwidth::Khz125 => 0x07,
            Bandwidth::Khz250 => 0x08,
            Bandwidth::Khz500 => 0x09,
        }
    }

    /// Hz, used only to decide whether LowDataRateOptimize is required.
    pub fn hz(self) -> u32 {
        match self {
            Bandwidth::Khz7_8 => 7_800,
            Bandwidth::Khz10_4 => 10_400,
            Bandwidth::Khz15_6 => 15_600,
            Bandwidth::Khz20_8 => 20_800,
            Bandwidth::Khz31_25 => 31_250,
            Bandwidth::Khz41_7 => 41_700,
            Bandwidth::Khz62_5 => 62_500,
            Bandwidth::Khz125 => 125_000,
            Bandwidth::Khz250 => 250_000,
            Bandwidth::Khz500 => 500_000,
        }
    }

    /// Inverse of `hz()` — the register only accepts these exact 10 values,
    /// so any other input (a typo, a stale/corrupt saved setting) is
    /// rejected rather than silently rounded to the nearest one. Single
    /// source of truth for every caller that accepts a raw Hz value from a
    /// human (a CLI flag, a config portal form field) instead of the enum
    /// directly.
    pub fn from_hz(hz: u32) -> Option<Self> {
        match hz {
            7_800 => Some(Bandwidth::Khz7_8),
            10_400 => Some(Bandwidth::Khz10_4),
            15_600 => Some(Bandwidth::Khz15_6),
            20_800 => Some(Bandwidth::Khz20_8),
            31_250 => Some(Bandwidth::Khz31_25),
            41_700 => Some(Bandwidth::Khz41_7),
            62_500 => Some(Bandwidth::Khz62_5),
            125_000 => Some(Bandwidth::Khz125),
            250_000 => Some(Bandwidth::Khz250),
            500_000 => Some(Bandwidth::Khz500),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CodingRate {
    Cr4_5,
    Cr4_6,
    Cr4_7,
    Cr4_8,
}

impl CodingRate {
    pub fn register_value(self) -> u8 {
        match self {
            CodingRate::Cr4_5 => 0x01,
            CodingRate::Cr4_6 => 0x02,
            CodingRate::Cr4_7 => 0x03,
            CodingRate::Cr4_8 => 0x04,
        }
    }

    /// The "5" in "4/5" — how this project's CLI/config surfaces coding
    /// rate to a human (a single digit) instead of the `Cr4_5` spelling.
    pub fn denominator(self) -> u8 {
        match self {
            CodingRate::Cr4_5 => 5,
            CodingRate::Cr4_6 => 6,
            CodingRate::Cr4_7 => 7,
            CodingRate::Cr4_8 => 8,
        }
    }

    /// Inverse of `denominator()` — see `Bandwidth::from_hz`'s doc comment
    /// for why this rejects anything outside the 4 supported values rather
    /// than clamping.
    pub fn from_denominator(d: u8) -> Option<Self> {
        match d {
            5 => Some(CodingRate::Cr4_5),
            6 => Some(CodingRate::Cr4_6),
            7 => Some(CodingRate::Cr4_7),
            8 => Some(CodingRate::Cr4_8),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub freq_hz: u32,
    /// This node's own address, embedded as a 2-byte prefix ahead of every
    /// outgoing payload so the receiver knows who sent it (raw LoRa has no
    /// addressing of its own).
    pub addr: u16,
    /// 6-12. SF6 requires implicit header mode, which this driver does not
    /// implement, so only 7-12 are accepted by `to_modem_config`.
    pub spreading_factor: u8,
    pub bandwidth: Bandwidth,
    pub coding_rate: CodingRate,
    pub sync_word: u8,
    pub preamble_len: u16,
    /// 2-20 dBm (PA_BOOST path; every RFM9x/Ra-02-style module wires only
    /// PA_BOOST to the antenna, not RFO).
    pub tx_power_dbm: i8,
    pub crc_on: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            freq_hz: 433_000_000,
            addr: 0,
            spreading_factor: 7,
            bandwidth: Bandwidth::Khz125,
            coding_rate: CodingRate::Cr4_5,
            sync_word: 0x12,
            preamble_len: 8,
            tx_power_dbm: 17,
            crc_on: true,
        }
    }
}

impl Config {
    /// Frf register value: Frf = freq_hz * 2^19 / F_XOSC (F_XOSC = 32MHz).
    pub fn frf_register(&self) -> u32 {
        (((self.freq_hz as u64) << 19) / 32_000_000) as u32
    }

    /// (RegPaConfig, RegPaDac) for the configured tx_power_dbm, PA_BOOST path.
    /// Pout = 17 - (15 - OutputPower) normally, or up to 20dBm with the PaDac
    /// boost enabled (RegPaDac = 0x87) at OutputPower = 15.
    pub fn pa_config_bytes(&self) -> (u8, u8) {
        let dbm = self.tx_power_dbm.clamp(2, 20);
        if dbm > 17 {
            (0x80 | 0x0F, 0x87)
        } else {
            let output_power = (dbm - 2).clamp(0, 15) as u8;
            (0x80 | output_power, 0x84)
        }
    }

    /// LoRa symbol period in microseconds: `2^SF / BW`. The basis for every
    /// timing figure in this module — `LowDataRateOptimize`'s own
    /// threshold, CAD duration, and on-air transmission time all scale
    /// directly from this.
    pub fn symbol_period_us(&self) -> u64 {
        (1u64 << self.spreading_factor) * 1_000_000 / self.bandwidth.hz() as u64
    }

    /// LowDataRateOptimize must be set when the symbol period exceeds 16ms.
    /// Computed in microseconds so the classic SF11/125kHz case (16.384ms)
    /// doesn't get truncated down to exactly 16ms by integer division.
    pub fn low_data_rate_optimize(&self) -> bool {
        self.symbol_period_us() > 16_000
    }

    /// A generous upper bound on how long Channel Activity Detection can
    /// take at this SF/BW, for use as `wait_for`'s CAD timeout. Per the
    /// datasheet, actual CAD duration is roughly 1.5-2 symbol periods; ×3
    /// leaves real margin for a software-polled (not cycle-precise) wait
    /// without risking a false timeout against a perfectly healthy radio —
    /// this replaces what used to be a fixed iteration count that was only
    /// ever implicitly tuned for SF7's much shorter symbol period, and
    /// silently stopped being enough once SF11 became the default (see
    /// docs/protocols/lora_online_control_protocol.md, "RF Parameters").
    pub fn cad_timeout_us(&self) -> u32 {
        (self.symbol_period_us() * 3).min(u32::MAX as u64) as u32
    }

    /// Estimated on-air transmission time for a `payload_len`-byte packet
    /// at this Config (explicit header, this driver's own CRC setting),
    /// per the standard LoRa time-on-air formula (Semtech AN1200.22).
    /// Integer microseconds throughout — no floating point, so this stays
    /// usable in a `no_std` context — with the preamble's `+4.25` symbol
    /// term handled as a ×4 fixed-point value (17/4) rather than rounding
    /// it away.
    ///
    /// Cross-checked against docs/protocols/lora_online_control_protocol.md's
    /// own "RF Parameters" airtime figures: a ~30-byte punch frame comes out
    /// to ~71.9ms at SF7/125kHz (doc: "~70ms") and ~905ms at SF11/125kHz
    /// (doc: "~900ms") — both computed independently by hand when those
    /// figures were first written, and reproduced exactly here.
    pub fn tx_airtime_us(&self, payload_len: usize) -> u64 {
        let ts_us = self.symbol_period_us();
        let sf = self.spreading_factor as i64;
        let de: i64 = if self.low_data_rate_optimize() { 1 } else { 0 };
        let crc: i64 = if self.crc_on { 1 } else { 0 };
        let cr = self.coding_rate.denominator() as i64 - 4; // 1..4 for 4/5..4/8
        let pl = payload_len as i64;

        // (preamble_len + 4.25) symbols, kept in integer microseconds via
        // ×4 fixed-point (4.25 == 17/4) instead of floating point.
        let preamble_us = (self.preamble_len as i64 * 4 + 17) as u64 * ts_us / 4;

        let numerator = 8 * pl - 4 * sf + 28 + 16 * crc;
        let denominator = 4 * (sf - 2 * de);
        let payload_symbols = if numerator > 0 && denominator > 0 {
            let ceil_div = (numerator + denominator - 1) / denominator;
            8 + (ceil_div * (cr + 4)).max(0)
        } else {
            8
        };

        preamble_us + payload_symbols as u64 * ts_us
    }

    /// A generous upper bound for `wait_for`'s TxDone timeout — actual
    /// estimated airtime (`tx_airtime_us`) plus margin, since this is a
    /// software-polled timeout, not a precise measurement of when the
    /// radio itself considers the transmission done.
    pub fn tx_timeout_us(&self, payload_len: usize) -> u32 {
        (self.tx_airtime_us(payload_len) * 2).min(u32::MAX as u64) as u32
    }

    /// (RegDetectionOptimize, RegDetectionThreshold) per datasheet section 4.1.1.6.
    pub fn detection_registers(&self) -> (u8, u8) {
        if self.spreading_factor == 6 {
            (0x05, 0x0C)
        } else {
            (0x03, 0x0A)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checked against a 30-byte punch frame's airtime as already
    /// published in docs/protocols/lora_online_control_protocol.md's "RF
    /// Parameters" section ("~70ms" at SF7, "~900ms" at SF11) — both
    /// figures were computed by hand when that doc was written, and this
    /// test reproduces the exact same numbers from the actual
    /// implementation, not just the same ballpark.
    #[test]
    fn test_tx_airtime_matches_published_punch_frame_figures() {
        let sf7 = Config { spreading_factor: 7, bandwidth: Bandwidth::Khz125, ..Default::default() };
        assert_eq!(sf7.tx_airtime_us(30), 71_936); // 71.936ms

        let sf11 = Config { spreading_factor: 11, bandwidth: Bandwidth::Khz125, ..Default::default() };
        assert_eq!(sf11.tx_airtime_us(30), 905_216); // 905.216ms
    }

    #[test]
    fn test_tx_airtime_increases_with_spreading_factor() {
        let base = Config { bandwidth: Bandwidth::Khz125, ..Default::default() };
        let mut last = 0;
        for sf in 7..=12 {
            let airtime = Config { spreading_factor: sf, ..base.clone() }.tx_airtime_us(20);
            assert!(airtime > last, "SF{} airtime {} should exceed SF{}'s {}", sf, airtime, sf - 1, last);
            last = airtime;
        }
    }

    #[test]
    fn test_tx_airtime_increases_with_payload_length() {
        let cfg = Config { spreading_factor: 9, bandwidth: Bandwidth::Khz125, ..Default::default() };
        assert!(cfg.tx_airtime_us(50) > cfg.tx_airtime_us(10));
    }

    #[test]
    fn test_cad_timeout_scales_with_symbol_period() {
        let sf7 = Config { spreading_factor: 7, bandwidth: Bandwidth::Khz125, ..Default::default() };
        let sf11 = Config { spreading_factor: 11, bandwidth: Bandwidth::Khz125, ..Default::default() };
        // SF11's symbol period is 16x SF7's (2^11 / 2^7) — the timeout
        // should scale the same way, not stay fixed.
        assert_eq!(sf11.cad_timeout_us(), sf7.cad_timeout_us() * 16);
        assert_eq!(sf7.cad_timeout_us(), (1024 * 3) as u32); // 1.024ms symbol × 3
    }

    #[test]
    fn test_tx_timeout_is_generous_margin_over_airtime() {
        let cfg = Config { spreading_factor: 10, bandwidth: Bandwidth::Khz125, ..Default::default() };
        let airtime = cfg.tx_airtime_us(20);
        assert_eq!(cfg.tx_timeout_us(20) as u64, airtime * 2);
    }

    #[test]
    fn test_bandwidth_from_hz_round_trips_every_variant() {
        for bw in [
            Bandwidth::Khz7_8, Bandwidth::Khz10_4, Bandwidth::Khz15_6, Bandwidth::Khz20_8,
            Bandwidth::Khz31_25, Bandwidth::Khz41_7, Bandwidth::Khz62_5, Bandwidth::Khz125,
            Bandwidth::Khz250, Bandwidth::Khz500,
        ] {
            assert_eq!(Bandwidth::from_hz(bw.hz()), Some(bw));
        }
    }

    #[test]
    fn test_bandwidth_from_hz_rejects_unsupported_value() {
        assert_eq!(Bandwidth::from_hz(100_000), None);
    }

    #[test]
    fn test_coding_rate_from_denominator_round_trips_every_variant() {
        for cr in [CodingRate::Cr4_5, CodingRate::Cr4_6, CodingRate::Cr4_7, CodingRate::Cr4_8] {
            assert_eq!(CodingRate::from_denominator(cr.denominator()), Some(cr));
        }
    }

    #[test]
    fn test_coding_rate_from_denominator_rejects_unsupported_value() {
        assert_eq!(CodingRate::from_denominator(9), None);
    }

    #[test]
    fn test_frf_register_433mhz() {
        let cfg = Config { freq_hz: 433_000_000, ..Default::default() };
        // Datasheet example: 434MHz -> 0x6C8000. 433MHz should be a bit lower.
        assert!(cfg.frf_register() > 0x6C0000 && cfg.frf_register() < 0x6C8000);
    }

    #[test]
    fn test_pa_config_17dbm() {
        let cfg = Config { tx_power_dbm: 17, ..Default::default() };
        assert_eq!(cfg.pa_config_bytes(), (0x8F, 0x84));
    }

    #[test]
    fn test_pa_config_20dbm_enables_boost() {
        let cfg = Config { tx_power_dbm: 20, ..Default::default() };
        assert_eq!(cfg.pa_config_bytes(), (0x8F, 0x87));
    }

    #[test]
    fn test_low_data_rate_optimize_sf11_125khz() {
        // 2^11 / 125000 * 1000 = 16.384ms -> just over the 16ms threshold.
        let cfg = Config { spreading_factor: 11, bandwidth: Bandwidth::Khz125, ..Default::default() };
        assert!(cfg.low_data_rate_optimize());
    }

    #[test]
    fn test_low_data_rate_optimize_sf7_125khz_not_needed() {
        let cfg = Config { spreading_factor: 7, bandwidth: Bandwidth::Khz125, ..Default::default() };
        assert!(!cfg.low_data_rate_optimize());
    }
}
