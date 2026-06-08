#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxPower {
    Dbm22,
    Dbm17,
    Dbm13,
    Dbm10,
}

impl TxPower {
    fn register_value(self) -> u8 {
        match self {
            TxPower::Dbm22 => 0x00,
            TxPower::Dbm17 => 0x01,
            TxPower::Dbm13 => 0x02,
            TxPower::Dbm10 => 0x03,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AirSpeed {
    Bps1200,
    Bps2400,
    Bps4800,
    Bps9600,
    Bps19200,
    Bps38400,
    Bps62500,
}

impl AirSpeed {
    fn register_value(self) -> u8 {
        match self {
            AirSpeed::Bps1200  => 0x01,
            AirSpeed::Bps2400  => 0x02,
            AirSpeed::Bps4800  => 0x03,
            AirSpeed::Bps9600  => 0x04,
            AirSpeed::Bps19200 => 0x05,
            AirSpeed::Bps38400 => 0x06,
            AirSpeed::Bps62500 => 0x07,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BufferSize {
    Bytes240,
    Bytes128,
    Bytes64,
    Bytes32,
}

impl BufferSize {
    fn register_value(self) -> u8 {
        match self {
            BufferSize::Bytes240 => 0x00,
            BufferSize::Bytes128 => 0x40,
            BufferSize::Bytes64  => 0x80,
            BufferSize::Bytes32  => 0xC0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub freq_mhz: u32,
    pub addr: u16,
    pub net_id: u8,
    pub power: TxPower,
    pub air_speed: AirSpeed,
    pub buffer_size: BufferSize,
    pub rssi: bool,
    pub crypt: u16,
}

impl Config {
    fn freq_offset(freq_mhz: u32) -> Option<(u32, u8)> {
        if freq_mhz >= 850 && freq_mhz <= 930 {
            Some((850, (freq_mhz - 850) as u8))
        } else if freq_mhz >= 410 && freq_mhz <= 493 {
            Some((410, (freq_mhz - 410) as u8))
        } else {
            None
        }
    }

    pub fn to_registers(&self) -> [u8; 12] {
        self.to_registers_checked().expect("freq_mhz out of supported range")
    }

    pub fn to_registers_checked(&self) -> Option<[u8; 12]> {
        let (_start, offset) = Self::freq_offset(self.freq_mhz)?;
        let rssi_flag = if self.rssi { 0x80 } else { 0x00 };
        Some([
            0xC0,
            0x00,
            0x09,
            (self.addr >> 8) as u8,
            (self.addr & 0xFF) as u8,
            self.net_id,
            0x60 | self.air_speed.register_value(),
            self.buffer_size.register_value() | self.power.register_value(),
            offset,
            0x40 | rssi_flag,
            (self.crypt >> 8) as u8,
            (self.crypt & 0xFF) as u8,
        ])
    }

    pub fn freq_offset_byte(&self) -> u8 {
        Self::freq_offset(self.freq_mhz).map(|(_, o)| o).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> Config {
        Config {
            freq_mhz: 868,
            addr: 0,
            net_id: 0,
            power: TxPower::Dbm22,
            air_speed: AirSpeed::Bps2400,
            buffer_size: BufferSize::Bytes240,
            rssi: true,
            crypt: 0,
        }
    }

    #[test]
    fn test_register_header() {
        let regs = default_config().to_registers();
        assert_eq!(regs[0], 0xC0);
        assert_eq!(regs[1], 0x00);
        assert_eq!(regs[2], 0x09);
    }

    #[test]
    fn test_addr_encoding() {
        let mut cfg = default_config();
        cfg.addr = 0x0102;
        let regs = cfg.to_registers();
        assert_eq!(regs[3], 0x01);
        assert_eq!(regs[4], 0x02);
    }

    #[test]
    fn test_freq_encoding_850_band() {
        let regs = default_config().to_registers();
        assert_eq!(regs[8], 18); // 868 - 850 = 18
    }

    #[test]
    fn test_freq_encoding_410_band() {
        let mut cfg = default_config();
        cfg.freq_mhz = 433;
        let regs = cfg.to_registers();
        assert_eq!(regs[8], 23); // 433 - 410 = 23
    }

    #[test]
    fn test_air_speed_2400() {
        let regs = default_config().to_registers();
        assert_eq!(regs[6], 0x60 | 0x02);
    }

    #[test]
    fn test_power_22dbm() {
        let regs = default_config().to_registers();
        assert_eq!(regs[7], 0x00); // buffer 240 (0x00) | power 22 (0x00)
    }

    #[test]
    fn test_rssi_enabled() {
        let regs = default_config().to_registers();
        assert_eq!(regs[9], 0x40 | 0x80);
    }

    #[test]
    fn test_rssi_disabled() {
        let mut cfg = default_config();
        cfg.rssi = false;
        let regs = cfg.to_registers();
        assert_eq!(regs[9], 0x40);
    }

    #[test]
    fn test_crypt_encoding() {
        let mut cfg = default_config();
        cfg.crypt = 0xABCD;
        let regs = cfg.to_registers();
        assert_eq!(regs[10], 0xAB);
        assert_eq!(regs[11], 0xCD);
    }

    #[test]
    fn test_buffer_size_32() {
        let mut cfg = default_config();
        cfg.buffer_size = BufferSize::Bytes32;
        let regs = cfg.to_registers();
        assert_eq!(regs[7] & 0xC0, 0xC0);
    }

    #[test]
    fn test_invalid_freq_returns_none() {
        let mut cfg = default_config();
        cfg.freq_mhz = 600;
        assert!(cfg.to_registers_checked().is_none());
    }
}
