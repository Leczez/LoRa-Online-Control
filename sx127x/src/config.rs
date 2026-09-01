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

    /// LowDataRateOptimize must be set when the symbol period exceeds 16ms.
    /// Computed in microseconds so the classic SF11/125kHz case (16.384ms)
    /// doesn't get truncated down to exactly 16ms by integer division.
    pub fn low_data_rate_optimize(&self) -> bool {
        let symbol_period_us = (1u64 << self.spreading_factor) * 1_000_000 / self.bandwidth.hz() as u64;
        symbol_period_us > 16_000
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
