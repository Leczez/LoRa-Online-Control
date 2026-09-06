//! Minimal radio smoke test for the ESP32-S3 Super Mini + RFM95W link.
//!
//! Scope: prove the SPI wiring and over-the-air config match lora-server on
//! the RPi side. It sends a text ping every few seconds and logs anything it
//! receives. It is deliberately NOT the full node from
//! docs/superpowers/specs/2026-08-25-esp32-si-punch-node-design.md — no USB
//! host, no OLED, no Wi-Fi config page. Those come after this link is
//! confirmed working on real hardware.

use std::time::Duration;

use embedded_hal::spi::MODE_0;
use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::prelude::*;
use esp_idf_hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver, SpiDriverConfig};

use sx127x::{Bandwidth, CodingRate, Config as RadioConfig, LoraRadio, Sx127xSpi};

// --- Must match whatever lora-server is actually launched with on the RPi
// (LORA_FREQ / LORA_SF / LORA_BW_HZ / LORA_CR / LORA_SYNC_WORD env vars /
// CLI flags, see lora-server/src/args.rs) — these are that crate's defaults.
const FREQ_HZ: u32 = 868_000_000;
const SPREADING_FACTOR: u8 = 7;
const BANDWIDTH: Bandwidth = Bandwidth::Khz125;
const CODING_RATE: CodingRate = CodingRate::Cr4_5;
const SYNC_WORD: u8 = 0x12;
const TX_POWER_DBM: i8 = 20;

// This node's own LoRa address and the RPi base station's address. Field
// nodes in the design doc use 10/11/12...; the RPi is conventionally 1
// (lora-server's own --dest default). Change NODE_ADDR per physical node.
const NODE_ADDR: u16 = 10;
const BASE_ADDR: u16 = 1;

fn main() -> anyhow::Result<()> {
    // Required on every esp-idf-svc std binary before touching any ESP-IDF
    // API — links libc/newlib patches the IDF needs.
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let pins = peripherals.pins;

    // Pin assignment matches the schematic's suggested wiring (Note 1: not
    // fixed, adjust freely if your board's silkscreen numbering differs).
    let sclk = pins.gpio12;
    let sdo = pins.gpio11; // MOSI
    let sdi = pins.gpio13; // MISO
    let cs = pins.gpio10; // NSS
    let reset = PinDriver::output(pins.gpio9)?;
    let dio0 = PinDriver::input(pins.gpio14)?;

    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        sclk,
        sdo,
        Some(sdi),
        &SpiDriverConfig::new(),
    )?;
    let spi = SpiDeviceDriver::new(
        spi_driver,
        Some(cs),
        &SpiConfig::new().baudrate(4.MHz().into()).data_mode(MODE_0),
    )?;

    let mut radio = Sx127xSpi::new_with_dio0(spi, reset, Delay::new_default(), dio0);

    let radio_config = RadioConfig {
        freq_hz: FREQ_HZ,
        addr: NODE_ADDR,
        spreading_factor: SPREADING_FACTOR,
        bandwidth: BANDWIDTH,
        coding_rate: CODING_RATE,
        sync_word: SYNC_WORD,
        tx_power_dbm: TX_POWER_DBM,
        ..Default::default()
    };
    radio
        .configure(&radio_config)
        .map_err(|e| anyhow::anyhow!("radio configure failed: {:?}", e))?;

    log::info!("esp32-node up: addr={NODE_ADDR} dest={BASE_ADDR} freq={FREQ_HZ}Hz sf={SPREADING_FACTOR}");

    let mut ping_count: u32 = 0;
    loop {
        match radio.receive() {
            Ok(Some(pkt)) => {
                let text = core::str::from_utf8(&pkt.payload).unwrap_or("<non-utf8>");
                log::info!("RX from {:#06x} rssi={:?}: {}", pkt.src_addr, pkt.rssi, text);
            }
            Ok(None) => {}
            Err(e) => log::warn!("receive() error: {:?}", e),
        }

        ping_count += 1;
        let payload = std::format!("PING {ping_count} from esp32-s3-super-mini");
        match radio.send(BASE_ADDR, payload.as_bytes()) {
            Ok(()) => log::info!("TX -> {:#06x}: {}", BASE_ADDR, payload),
            Err(e) => log::warn!("send() error: {:?}", e),
        }

        std::thread::sleep(Duration::from_secs(5));
    }
}
