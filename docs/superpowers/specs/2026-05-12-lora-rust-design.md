# LoRa Rust Rewrite — Design Spec

**Date:** 2026-05-12
**Status:** Approved

## Overview

Rewrite the existing Python LoRa SX126X software in Rust as a Cargo workspace. The goal is a clean, well-structured personal tool with a portable driver library and an interactive TUI application. Multi-platform support (Raspberry Pi, Linux desktop, ESP32 and other microcontrollers) is a first-class concern.

## Workspace Structure

```
lora-online-control/
├── Cargo.toml                (workspace manifest)
├── sx126x/                   (library crate — no_std driver)
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── config.rs         (register encoding, Config struct, typed enums)
│       ├── uart.rs           (UART transport driver)
│       └── spi.rs            (SPI transport driver)
└── lora-cli/                 (binary crate — std TUI app)
    ├── Cargo.toml
    └── src/
        └── main.rs
```

Embedded projects (e.g., ESP32 firmware) live in separate repos and pull in `sx126x` as a path or git dependency. They are not part of this workspace due to incompatible build toolchains.

## Library Crate: `sx126x`

### Platform Strategy

The library is `#![no_std]` with an optional `std` feature flag (enables `std::error::Error` impl). It is generic over hardware peripherals via [`embedded-hal`](https://crates.io/crates/embedded-hal) traits. This makes it portable to any platform that has an `embedded-hal` implementation:

| Platform | embedded-hal impl |
|---|---|
| Raspberry Pi | `rppal` |
| Linux desktop | `linux-embedded-hal` |
| ESP32 (ESP-IDF) | `esp-idf-hal` |
| ESP32 (bare-metal) | `esp-hal` |

### Transport Support

Two transport variants are supported:

**UART transport** (Waveshare HAT and similar UART-based modules):
```rust
pub struct Sx126xUart<UART, M0, M1> { ... }
// UART: embedded_io::Read + Write
// M0, M1: embedded_hal::digital::OutputPin (mode control pins)
```

**SPI transport** (direct SX126x chip connection):
```rust
pub struct Sx126xSpi<SPI, BUSY, RESET> { ... }
// SPI: embedded_hal::spi::SpiDevice (CS managed internally)
// BUSY, RESET: embedded_hal::digital::InputPin / OutputPin
```

On platforms without GPIO (plain Linux desktop via USB serial), a local `NoPin` stub implements `OutputPin` as a no-op.

### `LoraRadio` Trait

Both driver structs implement a common trait:

```rust
pub struct ReceivedPacket {
    pub src_addr: u16,
    pub rssi: Option<i16>,  // dBm, None if RSSI reporting disabled
    pub payload: heapless::Vec<u8, 240>,
}

pub trait LoraRadio {
    type Error;
    fn configure(&mut self, config: &Config) -> Result<(), Self::Error>;
    fn send(&mut self, dest: u16, payload: &[u8]) -> Result<(), Self::Error>;
    // Returns None if no message is currently available (non-blocking poll)
    fn receive(&mut self) -> Result<Option<ReceivedPacket>, Self::Error>;
}
```

`heapless::Vec` is used for the payload so the type works in `no_std` without a heap allocator. The max size (240) matches the largest SX126x packet buffer setting.

### `Config` Struct

All module settings are expressed as typed Rust values. Register byte encoding is encapsulated in `config.rs` and not exposed to callers.

```rust
pub struct Config {
    pub freq_mhz: u32,
    pub addr: u16,
    pub net_id: u8,
    pub power: TxPower,        // enum: 22/17/13/10 dBm
    pub air_speed: AirSpeed,   // enum: 1200..62500 bps
    pub buffer_size: BufferSize, // enum: 32/64/128/240 bytes
    pub rssi: bool,
    pub crypt: u16,
}
```

### Error Type

```rust
pub enum Sx126xError<E> {
    Transport(E),    // underlying serial/SPI hardware error
    InvalidConfig,   // unsupported parameter value
    Timeout,         // no response from module during configuration
}
```

No panics in library code. All fallible operations return `Result`.

## CLI Crate: `lora-cli`

### Startup

Configuration is passed via CLI flags at startup:

```
lora-cli --port /dev/ttyS0 --freq 868 --addr 0 --power 22
         [--m0-pin 22] [--m1-pin 27]   # RPi GPIO only, omit on desktop
```

### TUI Layout

Built with [`ratatui`](https://crates.io/crates/ratatui). Single interactive screen:

```
┌─ Config ─────────────────────────────────────────────┐
│ port: /dev/ttyS0  freq: 868MHz  addr: 0  power: 22dBm│
└──────────────────────────────────────────────────────┘
┌─ Traffic log ────────────────────────────────────────┐
│ [12:03:01] RX from 1  "Hello back"   RSSI: -82dBm   │
│ [12:03:05] TX to   1  "Hello World"                  │
│ [12:03:12] RX from 1  "ack"          RSSI: -80dBm   │
│                                                      │
└──────────────────────────────────────────────────────┘
┌─ Send ───────────────────────────────────────────────┐
│ > _                                                  │
└──────────────────────────────────────────────────────┘
```

- Traffic log scrolls automatically, newest at bottom
- Send bar accepts text input, sends on Enter
- `q` or `Esc` exits cleanly
- Panel borders use rounded corners

### Color Scheme

| Element | Color |
|---|---|
| RX messages | Green |
| TX messages | Cyan |
| RSSI > -90 dBm | Green |
| RSSI -90 to -110 dBm | Yellow |
| RSSI < -110 dBm | Red |
| Timestamps | Dim white |
| Config header labels | Bold white |
| Config header values | Yellow |
| Send bar border (focused) | Bright white |
| Errors | Red |

### Concurrency

A background thread runs the receive poll loop and sends incoming messages to the UI via an `mpsc` channel. The main thread drives the TUI event loop. Non-fatal errors (malformed packets) are logged in the traffic log in red. Fatal errors (serial disconnect) show a full-screen message and exit.

### Dependencies

| Crate | Purpose |
|---|---|
| `ratatui` | TUI rendering |
| `crossterm` | Terminal backend for ratatui |
| `clap` | CLI argument parsing (derive feature) |
| `rppal` | RPi GPIO + serial (RPi only, via feature flag) |
| `serialport` + `linux-embedded-hal` | Serial + embedded-hal impl (Linux desktop, via feature flag) |
| `heapless` | Stack-allocated Vec for no_std payload buffer |

`lora-cli` uses a Cargo feature flag (`rpi` vs default) to select the right hardware backend at compile time.

## Testing

- **`config.rs`** — pure unit tests asserting correct register bytes for given `Config` values. No hardware or mocking required.
- **Driver logic** — tested with [`embedded-hal-mock`](https://crates.io/crates/embedded-hal-mock): fake serial/SPI/GPIO with programmable responses. Covers send/receive framing, RSSI parsing, mode-pin sequencing.
- **CLI/TUI** — no automated tests. Thin glue code; verified by running on real hardware.
- **Hardware integration** — manual, run on RPi or target device.

## Out of Scope

- Publishing to crates.io
- ESP32 firmware project (separate repo, uses `sx126x` as a dependency)
- Config file support (`~/.config/lora-cli/config.toml`)
- Relay mode (present in Python code but not actively used)
