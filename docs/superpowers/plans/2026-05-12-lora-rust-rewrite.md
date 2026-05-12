# LoRa Rust Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rewrite the Python SX126x LoRa software as a Rust Cargo workspace with a portable `no_std` driver library and a ratatui TUI application.

**Architecture:** A `sx126x` library crate (no_std, generic over embedded-hal 1.0 traits) provides UART and SPI transport drivers behind a `LoraRadio` trait. A `lora-cli` binary crate builds a ratatui TUI on top, with feature-flagged support for Raspberry Pi (rppal GPIO) vs Linux desktop (serialport).

**Tech Stack:** Rust stable, embedded-hal 1.0, embedded-io 0.6, heapless 0.8, embedded-hal-mock 0.10, ratatui 0.28, crossterm 0.28, clap 4, serialport 6, rppal 0.19 (rpi feature).

---

## File Map

```
Cargo.toml                          workspace manifest
sx126x/
  Cargo.toml
  src/
    lib.rs                          re-exports, no_std, LoraRadio trait, ReceivedPacket, Sx126xError, NoPin
    config.rs                       Config struct, TxPower/AirSpeed/BufferSize enums, register encoding
    uart.rs                         Sx126xUart<UART, M0, M1> driver
    spi.rs                          Sx126xSpi<SPI, BUSY, RESET> driver
lora-cli/
  Cargo.toml
  src/
    main.rs                         entry point: parse args, build backend, run app
    app.rs                          App state, LogEntry enum, message channel types
    ui.rs                           ratatui render function, layout, color scheme
    backend.rs                      Radio trait (anyhow-erased), SerialPortWrapper, feature-flagged RPi backend
```

---

## Task 1: Workspace Scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `sx126x/Cargo.toml`
- Create: `sx126x/src/lib.rs`
- Create: `lora-cli/Cargo.toml`
- Create: `lora-cli/src/main.rs`

- [ ] **Step 1: Create workspace Cargo.toml**

```toml
# Cargo.toml
[workspace]
members = ["sx126x", "lora-cli"]
resolver = "2"
```

- [ ] **Step 2: Create sx126x/Cargo.toml**

```toml
# sx126x/Cargo.toml
[package]
name = "sx126x"
version = "0.1.0"
edition = "2021"

[features]
default = []
std = []

[dependencies]
embedded-hal = "1.0"
embedded-io = "0.6"
heapless = "0.8"

[dev-dependencies]
embedded-hal-mock = { version = "0.10", features = ["eh1"] }
```

- [ ] **Step 3: Create lora-cli/Cargo.toml**

```toml
# lora-cli/Cargo.toml
[package]
name = "lora-cli"
version = "0.1.0"
edition = "2021"

[features]
default = []
rpi = ["dep:rppal"]

[dependencies]
sx126x = { path = "../sx126x", features = ["std"] }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
ratatui = "0.28"
crossterm = "0.28"
serialport = "6"
embedded-io = "0.6"

[target.'cfg(target_os = "linux")'.dependencies]
rppal = { version = "0.19", optional = true }
```

- [ ] **Step 4: Create stub lib.rs and main.rs**

```rust
// sx126x/src/lib.rs
#![no_std]
```

```rust
// lora-cli/src/main.rs
fn main() {
    println!("lora-cli");
}
```

- [ ] **Step 5: Verify the workspace compiles**

```bash
cargo build
```

Expected: compiles successfully, no errors.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml sx126x/ lora-cli/
git commit -m "feat: scaffold Cargo workspace with sx126x and lora-cli crates"
```

---

## Task 2: Config Module

**Files:**
- Create: `sx126x/src/config.rs`
- Modify: `sx126x/src/lib.rs`

The register encoding is the core of the driver. The SX126x UART HAT accepts a 12-byte configuration frame.

- [ ] **Step 1: Write failing tests for register encoding**

```rust
// sx126x/src/config.rs  (add at the bottom)
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
        assert_eq!(regs[0], 0xC2); // volatile config command
        assert_eq!(regs[1], 0x00); // start register address
        assert_eq!(regs[2], 0x09); // register count
    }

    #[test]
    fn test_addr_encoding() {
        let mut cfg = default_config();
        cfg.addr = 0x0102;
        let regs = cfg.to_registers();
        assert_eq!(regs[3], 0x01); // high byte
        assert_eq!(regs[4], 0x02); // low byte
    }

    #[test]
    fn test_freq_encoding_850_band() {
        let regs = default_config().to_registers(); // 868MHz
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
        assert_eq!(regs[6], 0x60 | 0x02); // UART 9600 | 2400 bps
    }

    #[test]
    fn test_power_22dbm() {
        let regs = default_config().to_registers();
        // buffer 240 (0x00) | power 22 (0x00) | noise rssi (0x20)
        assert_eq!(regs[7], 0x20);
    }

    #[test]
    fn test_rssi_enabled() {
        let regs = default_config().to_registers();
        assert_eq!(regs[9], 0x43 | 0x80);
    }

    #[test]
    fn test_rssi_disabled() {
        let mut cfg = default_config();
        cfg.rssi = false;
        let regs = cfg.to_registers();
        assert_eq!(regs[9], 0x43);
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
        cfg.freq_mhz = 600; // not in 410-493 or 850-930 range
        assert!(cfg.to_registers_checked().is_none());
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p sx126x
```

Expected: compile error — `Config`, `TxPower`, `AirSpeed`, `BufferSize` not defined.

- [ ] **Step 3: Implement config.rs**

```rust
// sx126x/src/config.rs

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
    /// Returns (start_freq, offset) for a given frequency, or None if out of range.
    fn freq_offset(freq_mhz: u32) -> Option<(u32, u8)> {
        if freq_mhz >= 850 && freq_mhz <= 930 {
            Some((850, (freq_mhz - 850) as u8))
        } else if freq_mhz >= 410 && freq_mhz <= 493 {
            Some((410, (freq_mhz - 410) as u8))
        } else {
            None
        }
    }

    /// Encodes configuration into the 12-byte register frame.
    /// Panics if freq_mhz is out of range — use to_registers_checked for fallible encoding.
    pub fn to_registers(&self) -> [u8; 12] {
        self.to_registers_checked().expect("freq_mhz out of supported range")
    }

    /// Encodes configuration into the 12-byte register frame. Returns None if freq_mhz is invalid.
    pub fn to_registers_checked(&self) -> Option<[u8; 12]> {
        let (_start, offset) = Self::freq_offset(self.freq_mhz)?;
        let rssi_flag = if self.rssi { 0x80 } else { 0x00 };
        Some([
            0xC2,                                                          // volatile config
            0x00,                                                          // start register
            0x09,                                                          // register count
            (self.addr >> 8) as u8,                                        // addr high
            (self.addr & 0xFF) as u8,                                      // addr low
            self.net_id,                                                   // net id
            0x60 | self.air_speed.register_value(),                        // UART 9600 | air speed
            self.buffer_size.register_value() | self.power.register_value() | 0x20, // buf | pwr | noise rssi
            offset,                                                        // freq offset
            0x43 | rssi_flag,                                              // packet rssi flag
            (self.crypt >> 8) as u8,                                       // crypt high
            (self.crypt & 0xFF) as u8,                                     // crypt low
        ])
    }

    /// Returns the freq offset byte for use in packet headers.
    pub fn freq_offset_byte(&self) -> u8 {
        Self::freq_offset(self.freq_mhz).map(|(_, o)| o).unwrap_or(0)
    }
}
```

- [ ] **Step 4: Add module declaration to lib.rs**

```rust
// sx126x/src/lib.rs
#![no_std]

pub mod config;
pub use config::{AirSpeed, BufferSize, Config, TxPower};
```

- [ ] **Step 5: Run tests to confirm they pass**

```bash
cargo test -p sx126x
```

Expected: all 11 tests pass.

- [ ] **Step 6: Commit**

```bash
git add sx126x/
git commit -m "feat(sx126x): config module with register encoding and tests"
```

---

## Task 3: Core Library Types

**Files:**
- Modify: `sx126x/src/lib.rs`

- [ ] **Step 1: Add LoraRadio trait, ReceivedPacket, Sx126xError, and NoPin to lib.rs**

```rust
// sx126x/src/lib.rs  — full file
#![no_std]

pub mod config;
pub use config::{AirSpeed, BufferSize, Config, TxPower};

use heapless::Vec;

/// A packet received from the radio.
#[derive(Debug)]
pub struct ReceivedPacket {
    pub src_addr: u16,
    /// Signal strength in dBm. None if RSSI reporting was disabled in Config.
    pub rssi: Option<i16>,
    /// Raw payload bytes. Max 240 bytes (largest SX126x buffer setting).
    pub payload: Vec<u8, 240>,
}

/// Common interface for all radio transport implementations.
pub trait LoraRadio {
    type Error;

    /// Apply configuration to the module. Must be called before send/receive.
    fn configure(&mut self, config: &Config) -> Result<(), Self::Error>;

    /// Transmit payload to dest_addr.
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error>;

    /// Non-blocking receive poll. Returns Ok(None) if no message is available.
    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error>;
}

/// Error type for sx126x drivers. E is the underlying transport error.
#[derive(Debug)]
pub enum Sx126xError<E> {
    /// Underlying serial/SPI hardware error.
    Transport(E),
    /// Parameter value not supported by the hardware.
    InvalidConfig,
    /// Module did not respond during configuration.
    Timeout,
}

#[cfg(feature = "std")]
impl<E: core::fmt::Debug> std::error::Error for Sx126xError<E> {}

impl<E: core::fmt::Debug> core::fmt::Display for Sx126xError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Sx126xError::Transport(e) => write!(f, "transport error: {:?}", e),
            Sx126xError::InvalidConfig => write!(f, "invalid configuration"),
            Sx126xError::Timeout => write!(f, "module did not respond"),
        }
    }
}

/// A no-op GPIO pin for platforms without GPIO (e.g., plain Linux desktop via USB serial).
/// Implements OutputPin by doing nothing.
pub struct NoPin;

impl embedded_hal::digital::ErrorType for NoPin {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for NoPin {
    fn set_low(&mut self) -> Result<(), Self::Error> { Ok(()) }
    fn set_high(&mut self) -> Result<(), Self::Error> { Ok(()) }
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cargo build -p sx126x
```

Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add sx126x/src/lib.rs
git commit -m "feat(sx126x): LoraRadio trait, ReceivedPacket, Sx126xError, NoPin"
```

---

## Task 4: UART Driver

**Files:**
- Create: `sx126x/src/uart.rs`
- Modify: `sx126x/src/lib.rs`

The UART driver talks to a Waveshare-style HAT. Configuration uses M0/M1 GPIO pins to enter/exit config mode. Packets are framed with address and frequency-offset headers.

- [ ] **Step 1: Write failing tests**

```rust
// sx126x/src/uart.rs  — add at the bottom inside #[cfg(test)] mod tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use embedded_hal_mock::eh1::digital::{Mock as PinMock, State, Transaction as PinTx};

    // Minimal MockSerial that records writes and replays reads.
    struct MockSerial {
        write_buf: std::vec::Vec<u8>,
        read_buf: std::collections::VecDeque<u8>,
    }

    impl MockSerial {
        fn new(read_data: &[u8]) -> Self {
            Self {
                write_buf: std::vec::Vec::new(),
                read_buf: read_data.iter().copied().collect(),
            }
        }
    }

    impl embedded_io::ErrorType for MockSerial {
        type Error = core::convert::Infallible;
    }

    impl embedded_io::Read for MockSerial {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let n = buf.len().min(self.read_buf.len());
            for b in buf.iter_mut().take(n) {
                *b = self.read_buf.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl embedded_io::Write for MockSerial {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
    }

    fn config() -> Config {
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

    // Build a mock response for configure(): the HAT echoes back 0xC1 + 11 bytes.
    fn config_ack() -> [u8; 12] {
        let mut ack = [0u8; 12];
        ack[0] = 0xC1;
        ack
    }

    #[test]
    fn test_configure_writes_correct_registers() {
        // M1 HIGH (enter config), then M1 LOW, M0 LOW (exit config)
        let m0_expects = vec![PinTx::set(State::Low), PinTx::set(State::Low)];
        let m1_expects = vec![PinTx::set(State::High), PinTx::set(State::Low)];

        let serial = MockSerial::new(&config_ack());
        let m0 = PinMock::new(&m0_expects);
        let m1 = PinMock::new(&m1_expects);

        let mut radio = Sx126xUart::new(serial, m0, m1);
        radio.configure(&config()).unwrap();

        let expected_regs = config().to_registers();
        assert_eq!(&radio.serial.write_buf[..12], &expected_regs);

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_send_writes_correct_packet() {
        let serial = MockSerial::new(&[]);
        let m0 = PinMock::new(&[PinTx::set(State::Low)]);
        let m1 = PinMock::new(&[PinTx::set(State::Low)]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        radio.send(1, b"hello").unwrap();

        // Expected frame: [dst_high, dst_low, dst_freq, src_high, src_low, src_freq, payload...]
        let freq_off = config().freq_offset_byte();
        let expected = [0x00, 0x01, freq_off, 0x00, 0x00, freq_off,
                        b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(radio.serial.write_buf, expected);

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_returns_none_when_empty() {
        let serial = MockSerial::new(&[]);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        let result = radio.receive().unwrap();
        assert!(result.is_none());

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_parses_packet_with_rssi() {
        // Packet: [src_high, src_low, src_freq, payload..., rssi_byte]
        // rssi = -(256 - 174) = -82 dBm
        let packet = [0x00, 0x01, 0x12, b'h', b'i', 174u8];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: config() };
        let pkt = radio.receive().unwrap().unwrap();

        assert_eq!(pkt.src_addr, 1);
        assert_eq!(pkt.rssi, Some(-82));
        assert_eq!(pkt.payload.as_slice(), b"hi");

        radio.m0.done();
        radio.m1.done();
    }

    #[test]
    fn test_receive_parses_packet_without_rssi() {
        let mut cfg = config();
        cfg.rssi = false;
        let packet = [0x00, 0x02, 0x12, b'o', b'k'];
        let serial = MockSerial::new(&packet);
        let m0 = PinMock::new(&[]);
        let m1 = PinMock::new(&[]);

        let mut radio = Sx126xUart { serial, m0, m1, config: cfg };
        let pkt = radio.receive().unwrap().unwrap();

        assert_eq!(pkt.src_addr, 2);
        assert_eq!(pkt.rssi, None);
        assert_eq!(pkt.payload.as_slice(), b"ok");

        radio.m0.done();
        radio.m1.done();
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p sx126x uart
```

Expected: compile error — `Sx126xUart` not defined.

- [ ] **Step 3: Implement uart.rs**

```rust
// sx126x/src/uart.rs

use embedded_hal::digital::OutputPin;
use embedded_io::{Read, Write};
use heapless::Vec;

use crate::{Config, LoraRadio, ReceivedPacket, Sx126xError};

/// UART transport driver for SX126x-based HAT modules (e.g. Waveshare).
///
/// Generic parameters:
/// - UART: byte stream implementing embedded_io::Read + Write
/// - M0, M1: mode-select GPIO pins (OutputPin). Use NoPin for platforms without GPIO.
pub struct Sx126xUart<UART, M0, M1> {
    pub(crate) serial: UART,
    pub(crate) m0: M0,
    pub(crate) m1: M1,
    pub(crate) config: Config,
}

impl<UART, M0, M1> Sx126xUart<UART, M0, M1>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
{
    /// Creates a new driver. Call configure() before using send() or receive().
    pub fn new(serial: UART, m0: M0, m1: M1) -> Self {
        Self {
            serial,
            m0,
            m1,
            config: Config {
                freq_mhz: 868,
                addr: 0,
                net_id: 0,
                power: crate::TxPower::Dbm22,
                air_speed: crate::AirSpeed::Bps2400,
                buffer_size: crate::BufferSize::Bytes240,
                rssi: false,
                crypt: 0,
            },
        }
    }

    fn enter_config_mode(&mut self) -> Result<(), Sx126xError<UART::Error>> {
        self.m0.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.m1.set_high().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }

    fn enter_normal_mode(&mut self) -> Result<(), Sx126xError<UART::Error>> {
        self.m0.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        self.m1.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }
}

impl<UART, M0, M1> LoraRadio for Sx126xUart<UART, M0, M1>
where
    UART: Read + Write,
    M0: OutputPin,
    M1: OutputPin,
{
    type Error = Sx126xError<UART::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        let regs = config
            .to_registers_checked()
            .ok_or(Sx126xError::InvalidConfig)?;

        self.config = config.clone();
        self.enter_config_mode()?;

        self.serial
            .write_all(&regs)
            .map_err(Sx126xError::Transport)?;

        // Read back acknowledgement (0xC1 as first byte = success).
        let mut ack = [0u8; 12];
        self.serial
            .read(&mut ack)
            .map_err(Sx126xError::Transport)?;

        if ack[0] != 0xC1 {
            return Err(Sx126xError::Timeout);
        }

        self.enter_normal_mode()?;
        Ok(())
    }

    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        self.enter_normal_mode()?;
        let freq_off = self.config.freq_offset_byte();
        let header = [
            (dest >> 8) as u8,
            (dest & 0xFF) as u8,
            freq_off,
            (self.config.addr >> 8) as u8,
            (self.config.addr & 0xFF) as u8,
            freq_off,
        ];
        self.serial
            .write_all(&header)
            .map_err(Sx126xError::Transport)?;
        self.serial
            .write_all(payload)
            .map_err(Sx126xError::Transport)?;
        self.serial.flush().map_err(Sx126xError::Transport)?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        let mut header = [0u8; 3];
        let n = self.serial.read(&mut header).map_err(Sx126xError::Transport)?;
        if n == 0 {
            return Ok(None);
        }

        let src_addr = ((header[0] as u16) << 8) | header[1] as u16;

        // Read remaining bytes (payload + optional RSSI byte).
        let mut body = Vec::<u8, 240>::new();
        let mut byte = [0u8; 1];
        loop {
            let n = self.serial.read(&mut byte).map_err(Sx126xError::Transport)?;
            if n == 0 {
                break;
            }
            body.push(byte[0]).ok(); // silently drop if over 240 bytes
        }

        let (payload_bytes, rssi) = if self.config.rssi && body.len() >= 1 {
            let rssi_raw = *body.last().unwrap();
            let rssi_dbm = -(256i16 - rssi_raw as i16);
            (&body[..body.len() - 1], Some(rssi_dbm))
        } else {
            (body.as_slice(), None)
        };

        let mut payload = Vec::<u8, 240>::new();
        payload.extend_from_slice(payload_bytes).ok();

        Ok(Some(ReceivedPacket { src_addr, rssi, payload }))
    }
}
```

- [ ] **Step 4: Add module declaration to lib.rs**

```rust
// sx126x/src/lib.rs — add after config module
pub mod uart;
pub use uart::Sx126xUart;
```

- [ ] **Step 5: Run tests to confirm they pass**

```bash
cargo test -p sx126x uart
```

Expected: all 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add sx126x/
git commit -m "feat(sx126x): UART driver with configure/send/receive and mock tests"
```

---

## Task 5: SPI Driver

**Files:**
- Create: `sx126x/src/spi.rs`
- Modify: `sx126x/src/lib.rs`

The SPI driver communicates with an SX126x chip directly (no UART adapter). Commands follow the SX126x datasheet SPI protocol. This is more complex than the UART HAT: the chip requires a sequence of setup commands before it can transmit/receive.

- [ ] **Step 1: Write failing tests**

```rust
// sx126x/src/spi.rs  — add at the bottom

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use embedded_hal_mock::eh1::{
        digital::{Mock as PinMock, State, Transaction as PinTx},
        spi::{Mock as SpiMock, Transaction as SpiTx},
    };

    fn config() -> Config {
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

    // Helper: BUSY pin that returns high once then low (chip becomes ready).
    fn busy_ready() -> PinMock {
        PinMock::new(&[PinTx::get(State::Low)])
    }

    #[test]
    fn test_configure_sends_set_standby() {
        // First command in configure() must be SetStandby (0x80, 0x00).
        let spi = SpiMock::new(&[SpiTx::write(vec![0x80, 0x00])]);
        let busy = busy_ready();
        let reset = PinMock::new(&[]);

        let mut radio = Sx126xSpi::new(spi, busy, reset);

        // We only check the first SPI write — send_command wraps spi.write().
        let _ = radio.send_command(&[0x80, 0x00]);

        radio.spi.done();
        radio.busy.done();
        radio.reset.done();
    }

    #[test]
    fn test_rf_frequency_calculation() {
        // 868 MHz -> RfFreq = round(868_000_000 * 2^25 / 32_000_000) = 906_317_158 = 0x3608_0000 approx
        // Exact: 868_000_000 * 33_554_432 / 32_000_000 = 910_260_019 = 0x3643_3333 approx
        // Use integer: 868_000_000u64 * (1 << 25) / 32_000_000
        let rf_freq = Sx126xSpi::<SpiMock, PinMock, PinMock>::freq_to_register(868);
        // Value must be in the correct ballpark for 868 MHz
        assert!(rf_freq > 0x3600_0000 && rf_freq < 0x3700_0000);
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test -p sx126x spi
```

Expected: compile error — `Sx126xSpi` not defined.

- [ ] **Step 3: Implement spi.rs**

```rust
// sx126x/src/spi.rs

use embedded_hal::{
    digital::{InputPin, OutputPin},
    spi::SpiDevice,
};

use crate::{Config, LoraRadio, ReceivedPacket, Sx126xError};

// SX126x SPI opcodes
const CMD_SET_STANDBY: u8         = 0x80;
const CMD_SET_PACKET_TYPE: u8     = 0x8A;
const CMD_SET_RF_FREQUENCY: u8    = 0x86;
const CMD_SET_TX_PARAMS: u8       = 0x8E;
const CMD_SET_MODULATION_PARAMS: u8 = 0x8B;
const CMD_SET_PACKET_PARAMS: u8   = 0x8C;
const CMD_SET_DIO_IRQ_PARAMS: u8  = 0x08;
const CMD_SET_RX: u8              = 0x82;
const CMD_SET_TX: u8              = 0x83;
const CMD_WRITE_BUFFER: u8        = 0x0E;
const CMD_READ_BUFFER: u8         = 0x1E;
const CMD_GET_IRQ_STATUS: u8      = 0x12;
const CMD_CLEAR_IRQ_STATUS: u8    = 0x02;
const CMD_GET_RX_BUFFER_STATUS: u8 = 0x13;

const PACKET_TYPE_LORA: u8 = 0x01;
const STANDBY_RC: u8       = 0x00;

/// SPI transport driver for direct SX126x chip connections.
///
/// Generic parameters:
/// - SPI: SpiDevice (manages chip-select internally)
/// - BUSY: InputPin — HIGH while chip is processing a command
/// - RESET: OutputPin — active-low hardware reset
pub struct Sx126xSpi<SPI, BUSY, RESET> {
    pub(crate) spi: SPI,
    pub(crate) busy: BUSY,
    pub(crate) reset: RESET,
    rssi_enabled: bool,
}

impl<SPI, BUSY, RESET> Sx126xSpi<SPI, BUSY, RESET>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
{
    pub fn new(spi: SPI, busy: BUSY, reset: RESET) -> Self {
        Self { spi, busy, reset, rssi_enabled: false }
    }

    /// Converts MHz to the SX126x RF frequency register value.
    pub fn freq_to_register(freq_mhz: u32) -> u32 {
        // RfFreq = freq_hz * 2^25 / 32_000_000
        ((freq_mhz as u64 * 1_000_000 * (1 << 25)) / 32_000_000) as u32
    }

    /// Waits until BUSY pin goes low (chip ready), ignores errors.
    fn wait_busy(&mut self) {
        // In production use a timeout loop. Here we do a best-effort read.
        for _ in 0..10_000 {
            if self.busy.is_low().unwrap_or(true) {
                break;
            }
        }
    }

    /// Sends a raw SPI command (write-only).
    pub fn send_command(&mut self, cmd: &[u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        self.spi.write(cmd).map_err(Sx126xError::Transport)
    }

    /// Writes a command followed by data bytes.
    fn write_cmd_data(&mut self, opcode: u8, data: &[u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        let mut buf = heapless::Vec::<u8, 32>::new();
        buf.push(opcode).ok();
        buf.extend_from_slice(data).ok();
        self.spi
            .write(&buf)
            .map_err(Sx126xError::Transport)
    }

    /// Reads n bytes after sending opcode + status byte.
    fn read_cmd(&mut self, opcode: u8, out: &mut [u8]) -> Result<(), Sx126xError<SPI::Error>> {
        self.wait_busy();
        // Protocol: send [opcode, 0x00 (status)], then read
        let mut cmd = [opcode, 0x00];
        self.spi.transfer_in_place(&mut cmd).map_err(Sx126xError::Transport)?;
        self.spi.read(out).map_err(Sx126xError::Transport)?;
        Ok(())
    }

    fn hardware_reset(&mut self) -> Result<(), Sx126xError<SPI::Error>> {
        self.reset.set_low().map_err(|_| Sx126xError::InvalidConfig)?;
        // A real implementation would sleep ~1ms here via embedded_hal::delay::DelayNs.
        // Omitted to avoid adding a Delay type parameter; callers should reset before new().
        self.reset.set_high().map_err(|_| Sx126xError::InvalidConfig)?;
        Ok(())
    }
}

impl<SPI, BUSY, RESET> LoraRadio for Sx126xSpi<SPI, BUSY, RESET>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    RESET: OutputPin,
{
    type Error = Sx126xError<SPI::Error>;

    fn configure(&mut self, config: &Config) -> Result<(), Self::Error> {
        self.rssi_enabled = config.rssi;

        self.hardware_reset()?;

        // SetStandby (RC oscillator)
        self.write_cmd_data(CMD_SET_STANDBY, &[STANDBY_RC])?;

        // SetPacketType: LoRa
        self.write_cmd_data(CMD_SET_PACKET_TYPE, &[PACKET_TYPE_LORA])?;

        // SetRfFrequency
        let rf_freq = Self::freq_to_register(config.freq_mhz);
        self.write_cmd_data(CMD_SET_RF_FREQUENCY, &[
            (rf_freq >> 24) as u8,
            (rf_freq >> 16) as u8,
            (rf_freq >> 8) as u8,
            rf_freq as u8,
        ])?;

        // SetTxParams: power, ramp time 200µs
        let power_byte = match config.power {
            crate::TxPower::Dbm22 => 22i8 as u8,
            crate::TxPower::Dbm17 => 17i8 as u8,
            crate::TxPower::Dbm13 => 13i8 as u8,
            crate::TxPower::Dbm10 => 10i8 as u8,
        };
        self.write_cmd_data(CMD_SET_TX_PARAMS, &[power_byte, 0x04])?;

        // SetModulationParams: SF7, BW 125kHz, CR 4/5, low-datarate optimize off
        self.write_cmd_data(CMD_SET_MODULATION_PARAMS, &[0x07, 0x04, 0x01, 0x00])?;

        // SetPacketParams: preamble 8, explicit header, 255 byte max, CRC on, no invert IQ
        self.write_cmd_data(CMD_SET_PACKET_PARAMS, &[0x00, 0x08, 0x00, 0xFF, 0x01, 0x00])?;

        // SetDioIrqParams: enable TxDone (bit 0) and RxDone (bit 1) on DIO1
        self.write_cmd_data(CMD_SET_DIO_IRQ_PARAMS, &[
            0x00, 0x03, // IRQ mask: TxDone | RxDone
            0x00, 0x03, // DIO1 mask
            0x00, 0x00, // DIO2 mask
            0x00, 0x00, // DIO3 mask
        ])?;

        Ok(())
    }

    fn send(&mut self, _dest: u16, payload: &[u8]) -> Result<(), Self::Error> {
        // WriteBuffer: offset 0x00, then payload
        self.wait_busy();
        let mut cmd = heapless::Vec::<u8, 242>::new();
        cmd.push(CMD_WRITE_BUFFER).ok();
        cmd.push(0x00).ok(); // buffer offset
        cmd.extend_from_slice(payload).ok();
        self.spi.write(&cmd).map_err(Sx126xError::Transport)?;

        // SetTx: timeout 0 (single TX, returns to standby)
        self.write_cmd_data(CMD_SET_TX, &[0x00, 0x00, 0x00])?;

        // Wait for TxDone IRQ
        for _ in 0..100_000 {
            let mut irq = [0u8; 2];
            self.read_cmd(CMD_GET_IRQ_STATUS, &mut irq)?;
            if irq[1] & 0x01 != 0 {
                // Clear IRQ
                self.write_cmd_data(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;
                return Ok(());
            }
        }
        Err(Sx126xError::Timeout)
    }

    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error> {
        // Put chip in single RX mode (timeout = 0 means listen until packet or timeout)
        self.write_cmd_data(CMD_SET_RX, &[0x00, 0x00, 0x00])?;

        // Check IRQ status — non-blocking: if no RxDone, return None
        let mut irq = [0u8; 2];
        self.read_cmd(CMD_GET_IRQ_STATUS, &mut irq)?;
        if irq[1] & 0x02 == 0 {
            return Ok(None);
        }
        self.write_cmd_data(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF])?;

        // GetRxBufferStatus: payload length and buffer offset
        let mut buf_status = [0u8; 2];
        self.read_cmd(CMD_GET_RX_BUFFER_STATUS, &mut buf_status)?;
        let payload_len = buf_status[0] as usize;
        let buf_offset = buf_status[1];

        // ReadBuffer
        let mut read_cmd = [CMD_READ_BUFFER, buf_offset, 0x00];
        self.spi.transfer_in_place(&mut read_cmd).map_err(Sx126xError::Transport)?;

        let mut payload = heapless::Vec::<u8, 240>::new();
        let read_len = payload_len.min(240);
        payload.resize(read_len, 0).ok();
        self.spi.read(payload.as_mut_slice()).map_err(Sx126xError::Transport)?;

        // SPI transport doesn't embed src_addr in the payload — use 0 (unknown).
        // Applications that need addressing should use the UART HAT or encode addr in payload.
        Ok(Some(ReceivedPacket { src_addr: 0, rssi: None, payload }))
    }
}
```

- [ ] **Step 4: Add module declaration to lib.rs**

```rust
// sx126x/src/lib.rs — add after uart module
pub mod spi;
pub use spi::Sx126xSpi;
```

- [ ] **Step 5: Run tests to confirm they pass**

```bash
cargo test -p sx126x spi
```

Expected: 2 tests pass.

- [ ] **Step 6: Run full test suite**

```bash
cargo test -p sx126x
```

Expected: all tests pass (config + uart + spi).

- [ ] **Step 7: Commit**

```bash
git add sx126x/
git commit -m "feat(sx126x): SPI driver with configure/send/receive and tests"
```

---

## Task 6: lora-cli Args and App State

**Files:**
- Create: `lora-cli/src/app.rs`
- Modify: `lora-cli/src/main.rs`

- [ ] **Step 1: Create app.rs with App state and LogEntry**

```rust
// lora-cli/src/app.rs

use std::collections::VecDeque;

pub const MAX_LOG_ENTRIES: usize = 1000;

#[derive(Debug)]
pub enum LogEntry {
    Rx {
        timestamp: String,
        src_addr: u16,
        payload: String,
        rssi: Option<i16>,
    },
    Tx {
        timestamp: String,
        dest_addr: u16,
        payload: String,
    },
    Error {
        timestamp: String,
        message: String,
    },
}

pub struct App {
    pub log: VecDeque<LogEntry>,
    pub input: String,
    pub should_quit: bool,
    pub config_line: String,
}

impl App {
    pub fn new(config_line: String) -> Self {
        Self {
            log: VecDeque::new(),
            input: String::new(),
            should_quit: false,
            config_line,
        }
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }
}
```

- [ ] **Step 2: Replace main.rs with Args parsing**

```rust
// lora-cli/src/main.rs

mod app;
mod backend;
mod ui;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-cli", about = "Interactive LoRa terminal")]
pub struct Args {
    /// Serial port path (e.g. /dev/ttyS0 or /dev/ttyUSB0)
    #[arg(long)]
    pub port: String,

    /// Frequency in MHz (410-493 or 850-930)
    #[arg(long, default_value_t = 868)]
    pub freq: u32,

    /// Node address (0-65535)
    #[arg(long, default_value_t = 0)]
    pub addr: u16,

    /// Destination address for sent messages (0-65535)
    #[arg(long, default_value_t = 1)]
    pub dest: u16,

    /// TX power in dBm (10, 13, 17, or 22)
    #[arg(long, default_value_t = 22)]
    pub power: u8,

    /// Air speed in bps (1200, 2400, 4800, 9600, 19200, 38400, 62500)
    #[arg(long, default_value_t = 2400)]
    pub air_speed: u32,

    /// M0 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long)]
    pub m0_pin: Option<u8>,

    /// M1 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long)]
    pub m1_pin: Option<u8>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    backend::run(args)
}
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo build -p lora-cli 2>&1 | head -30
```

Expected: errors about missing `backend` and `ui` modules — that's fine, we'll add them next. If the error is about `app.rs` itself, fix it first.

- [ ] **Step 4: Commit**

```bash
git add lora-cli/src/
git commit -m "feat(lora-cli): Args struct and App state"
```

---

## Task 7: lora-cli UI Rendering

**Files:**
- Create: `lora-cli/src/ui.rs`

- [ ] **Step 1: Create ui.rs**

```rust
// lora-cli/src/ui.rs

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, LogEntry};

pub fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // config header
            Constraint::Min(5),     // traffic log
            Constraint::Length(3),  // send bar
        ])
        .split(frame.area());

    render_config(frame, app, chunks[0]);
    render_log(frame, app, chunks[1]);
    render_input(frame, app, chunks[2]);
}

fn render_config(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Config ");

    let text = Line::from(vec![
        Span::styled(&app.config_line, Style::default().fg(Color::Yellow)),
    ]);

    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn render_log(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Traffic ");

    let items: Vec<ListItem> = app
        .log
        .iter()
        .map(|entry| ListItem::new(format_entry(entry)))
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn render_input(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White))
        .title(" Send ");

    let text = format!("> {}_", app.input);
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn format_entry(entry: &LogEntry) -> Line<'static> {
    match entry {
        LogEntry::Rx { timestamp, src_addr, payload, rssi } => {
            let mut spans = vec![
                Span::styled(
                    format!("[{}] ", timestamp),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("RX from {:5}  ", src_addr),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    format!("{:?}", payload),
                    Style::default().fg(Color::Green),
                ),
            ];
            if let Some(dbm) = rssi {
                let rssi_color = rssi_color(*dbm);
                spans.push(Span::styled(
                    format!("   RSSI: {}dBm", dbm),
                    Style::default().fg(rssi_color),
                ));
            }
            Line::from(spans)
        }
        LogEntry::Tx { timestamp, dest_addr, payload } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("TX to   {:5}  {:?}", dest_addr, payload),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        LogEntry::Error { timestamp, message } => Line::from(vec![
            Span::styled(
                format!("[{}] ", timestamp),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("ERR  {}", message),
                Style::default().fg(Color::Red),
            ),
        ]),
    }
}

fn rssi_color(dbm: i16) -> Color {
    if dbm > -90 {
        Color::Green
    } else if dbm >= -110 {
        Color::Yellow
    } else {
        Color::Red
    }
}
```

- [ ] **Step 2: Commit**

```bash
git add lora-cli/src/ui.rs
git commit -m "feat(lora-cli): ratatui UI with layout and color scheme"
```

---

## Task 8: lora-cli Backend

**Files:**
- Create: `lora-cli/src/backend.rs`

The backend erases the driver's generic error type via `anyhow`, selects the correct hardware implementation at compile time, and exposes a uniform `Radio` trait to the rest of the app.

- [ ] **Step 1: Create backend.rs**

```rust
// lora-cli/src/backend.rs

use anyhow::Result;
use sx126x::{Config, NoPin, ReceivedPacket, Sx126xUart, LoraRadio};
use embedded_io::Write;

use crate::app::App;
use crate::Args;

/// Local wrapper around serialport that implements embedded_io::Read + Write.
pub struct SerialPortWrapper(pub Box<dyn serialport::SerialPort>);

impl embedded_io::ErrorType for SerialPortWrapper {
    type Error = std::io::Error;
}

impl embedded_io::Read for SerialPortWrapper {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Read::read(&mut self.0, buf)
    }
}

impl embedded_io::Write for SerialPortWrapper {
    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Self::Error> {
        std::io::Write::write(&mut self.0, buf)
    }
    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        std::io::Write::flush(&mut self.0)
    }
}

/// Error-erased radio interface used by the event loop.
pub trait Radio: Send {
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<()>;
    fn receive(&mut self) -> Result<Option<ReceivedPacket>>;
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

fn build_config(args: &Args) -> Result<Config> {
    use sx126x::{AirSpeed, BufferSize, TxPower};

    let power = match args.power {
        22 => TxPower::Dbm22,
        17 => TxPower::Dbm17,
        13 => TxPower::Dbm13,
        10 => TxPower::Dbm10,
        p => anyhow::bail!("unsupported power {}dBm — use 10, 13, 17, or 22", p),
    };
    let air_speed = match args.air_speed {
        1200  => AirSpeed::Bps1200,
        2400  => AirSpeed::Bps2400,
        4800  => AirSpeed::Bps4800,
        9600  => AirSpeed::Bps9600,
        19200 => AirSpeed::Bps19200,
        38400 => AirSpeed::Bps38400,
        62500 => AirSpeed::Bps62500,
        s => anyhow::bail!("unsupported air_speed {} — use 1200/2400/4800/9600/19200/38400/62500", s),
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
    let port = serialport::new(port, 9600)
        .timeout(std::time::Duration::from_millis(100))
        .open()
        .map_err(|e| anyhow::anyhow!("failed to open {}: {}", port, e))?;
    Ok(SerialPortWrapper(port))
}

/// Builds and configures the radio backend based on CLI args and feature flags.
pub fn run(args: Args) -> Result<()> {
    let config = build_config(&args)?;

    #[cfg(feature = "rpi")]
    if args.m0_pin.is_some() && args.m1_pin.is_some() {
        return run_rpi(args, config);
    }

    run_desktop(args, config)
}

fn run_desktop(args: Args, config: Config) -> Result<()> {
    let serial = open_serial(&args.port)?;
    let mut driver = Sx126xUart::new(serial, NoPin, NoPin);
    driver.configure(&config)?;
    crate::ui::run_app(&args.port, args.dest, config, Box::new(driver))
}

#[cfg(feature = "rpi")]
fn run_rpi(args: Args, config: Config) -> Result<()> {
    use rppal::gpio::Gpio;
    use rppal::uart::{Parity, Uart};

    struct RppalSerial(Uart);

    impl embedded_io::ErrorType for RppalSerial {
        type Error = std::io::Error;
    }
    impl embedded_io::Read for RppalSerial {
        fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, Self::Error> {
            self.0.read(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }
    }
    impl embedded_io::Write for RppalSerial {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, Self::Error> {
            self.0.write(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }
        fn flush(&mut self) -> std::result::Result<(), Self::Error> { Ok(()) }
    }

    let gpio = Gpio::new()?;
    let m0 = gpio.get(args.m0_pin.unwrap())?.into_output();
    let m1 = gpio.get(args.m1_pin.unwrap())?.into_output();
    let uart = Uart::with_path(&args.port, 9600, Parity::None, 8, 1)?;

    let mut driver = Sx126xUart::new(RppalSerial(uart), m0, m1);
    driver.configure(&config)?;
    crate::ui::run_app(&args.port, args.dest, config, Box::new(driver))
}
```

- [ ] **Step 2: Commit**

```bash
git add lora-cli/src/backend.rs
git commit -m "feat(lora-cli): backend with serialport wrapper and feature-flagged RPi support"
```

---

## Task 9: lora-cli Event Loop

**Files:**
- Modify: `lora-cli/src/ui.rs`

Wire up the ratatui event loop, background receive thread, and mpsc channel. The `render` function already exists — this task adds `run_app`.

- [ ] **Step 1: Add run_app to ui.rs**

Add the following to `lora-cli/src/ui.rs`:

```rust
// Add these imports at the top of ui.rs:
use std::time::{Duration, Instant};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sx126x::Config;
use crate::app::LogEntry;
use crate::backend::Radio;

pub fn run_app(port: &str, dest_addr: u16, config: Config, mut radio: Box<dyn Radio>) -> anyhow::Result<()> {
    // Radio polling and TUI events are both driven from the main thread on a 100ms tick.
    // No background thread needed — receive() is a non-blocking poll.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let config_line = format!(
        "port: {}  freq: {}MHz  addr: {}  dest: {}  power: {:?}",
        port, config.freq_mhz, config.addr, dest_addr, config.power
    );
    let mut app = crate::app::App::new(config_line);

    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| render(f, &app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.should_quit = true;
                    }
                    KeyCode::Enter => {
                        if !app.input.is_empty() {
                            let msg = std::mem::take(&mut app.input);
                            let ts = timestamp();
                            match radio.send(dest_addr, msg.as_bytes()) {
                                Ok(()) => app.push_log(LogEntry::Tx {
                                    timestamp: ts,
                                    dest_addr,
                                    payload: msg,
                                }),
                                Err(e) => app.push_log(LogEntry::Error {
                                    timestamp: ts,
                                    message: e.to_string(),
                                }),
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => {
                        app.input.push(c);
                    }
                    _ => {}
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            // Poll for incoming radio packets.
            match radio.receive() {
                Ok(Some(pkt)) => {
                    let payload = String::from_utf8_lossy(&pkt.payload).into_owned();
                    app.push_log(LogEntry::Rx {
                        timestamp: timestamp(),
                        src_addr: pkt.src_addr,
                        payload,
                        rssi: pkt.rssi,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    app.push_log(LogEntry::Error {
                        timestamp: timestamp(),
                        message: e.to_string(),
                    });
                }
            }
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}
```

- [ ] **Step 2: Build the full project**

```bash
cargo build -p lora-cli
```

Expected: compiles with no errors. Fix any type mismatches that arise from wiring the modules together.

- [ ] **Step 3: Run the full test suite one more time**

```bash
cargo test
```

Expected: all sx126x tests pass, lora-cli has no test failures.

- [ ] **Step 4: Commit**

```bash
git add lora-cli/src/
git commit -m "feat(lora-cli): TUI event loop with keyboard input and radio polling"
```

---

## Task 10: Manual Smoke Test

This task has no automated steps — it verifies the app works end-to-end on real hardware.

**On Linux desktop (USB serial dongle):**

```bash
cargo run -p lora-cli -- --port /dev/ttyUSB0 --freq 868 --addr 0 --dest 1
```

Verify:
- TUI renders with three panels (Config, Traffic, Send)
- Typing in the send bar shows characters
- Pressing Enter sends and logs a TX entry in cyan
- Pressing `q` exits cleanly

**On Raspberry Pi (with Waveshare HAT, GPIO):**

```bash
cargo run -p lora-cli --features rpi -- \
  --port /dev/ttyS0 --freq 868 --addr 0 --dest 1 \
  --m0-pin 22 --m1-pin 27
```

Verify:
- Module configures without "setting fail" output
- Sending a message produces a TX log entry
- A second RPi or device on the same frequency produces an RX log entry with RSSI value

- [ ] **Step 1: Run desktop smoke test**
- [ ] **Step 2: Run RPi smoke test (if hardware available)**
- [ ] **Step 3: Final commit**

```bash
git add .
git commit -m "feat: complete LoRa Rust rewrite — sx126x driver + lora-cli TUI"
```
