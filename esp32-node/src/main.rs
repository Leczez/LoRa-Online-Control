//! ESP32-S3 SportIdent punch relay node.
//!
//! No physical config switch: the node always boots into a 2-minute Wi-Fi
//! config portal first (see wifi_config.rs) — a technician can change
//! addr/dest/freq there without reflashing; if nothing is saved, Wi-Fi is
//! stopped and normal operation proceeds with whatever's in NVS (or these
//! defaults on first boot). Reads punches from the SI master over USB
//! (cp210x.rs + sportident.rs) and relays them to the base station over
//! LoRa, using the same wire format lora-server already parses.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use embedded_hal::spi::MODE_0;
use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::prelude::*;
use esp_idf_hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver, SpiDriverConfig};
use esp_idf_svc::eventloop::EspSystemEventLoop;

use sx127x::{Bandwidth, CodingRate, Config as RadioConfig, LoraRadio, Sx127xSpi};

mod config;
mod cp210x;
mod sportident;
mod wifi_config;

use config::NodeConfig;

// Fixed modem parameters, shared fleet-wide — not exposed via the config
// page (only addr/dest/freq are; see wifi_config.rs). Must match lora-3b-2's
// deployed /etc/lora-server/env.
const SPREADING_FACTOR: u8 = 7;
const BANDWIDTH: Bandwidth = Bandwidth::Khz125;
const CODING_RATE: CodingRate = CodingRate::Cr4_5;
const SYNC_WORD: u8 = 0x12;
const TX_POWER_DBM: i8 = 20;

// First-boot defaults, matching lora-3b-2's actual deployment. addr=10 is
// this node's own address; dest=2 targets lora-3b-2 directly (it's itself
// addr=2, a relay hop toward addr=1, not address 1 itself).
const DEFAULT_CONFIG: NodeConfig = NodeConfig { addr: 10, dest: 2, freq_hz: 433_000_000 };

fn main() -> anyhow::Result<()> {
    // Required on every esp-idf-svc std binary before touching any ESP-IDF
    // API — links libc/newlib patches the IDF needs.
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    let nvs = Arc::new(Mutex::new(config::open_nvs()?));
    let current = {
        let guard = nvs.lock().unwrap();
        NodeConfig::load(&guard, DEFAULT_CONFIG)
    };

    // Either returns after the window closes with `current` still accurate
    // (nothing saved), or a save inside the portal calls esp_restart()
    // directly and this call never returns at all.
    wifi_config::run(peripherals.modem, sysloop, Arc::clone(&nvs), current)?;

    let pins = peripherals.pins;

    // Pin assignment matches the schematic's suggested wiring (Note 1: not
    // fixed, adjust freely if your board's silkscreen numbering differs).
    let sclk = pins.gpio12;
    let sdo = pins.gpio11; // MOSI
    let sdi = pins.gpio13; // MISO
    let cs = pins.gpio10; // NSS
    let reset = PinDriver::output(pins.gpio9)?;
    // DIO0 (GPIO7 — GPIO14/15/16 are SMD probe points on this board, not
    // usable header pins, see the wiring doc) isn't wired yet — falls back
    // to SPI-register polling for TX/CAD completion (Sx127xSpi::new). Once
    // DIO0 is physically connected, switch to
    // Sx127xSpi::new_with_dio0(..., PinDriver::input(pins.gpio7)?) for
    // cheaper GPIO-based waiting instead.

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

    let mut radio = Sx127xSpi::new(spi, reset, Delay::new_default());

    let radio_config = RadioConfig {
        freq_hz: current.freq_hz,
        addr: current.addr,
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

    log::info!(
        "esp32-node up: addr={} dest={} freq={}Hz sf={}",
        current.addr, current.dest, current.freq_hz, SPREADING_FACTOR
    );

    cp210x::install()?;
    log::info!("waiting for SI master (VID {:#06x} PID {:#06x})...", sportident::SI_VID, sportident::SI_PID);
    let transport = cp210x::wait_for_si_master(sportident::SI_PID, sportident::SI_BAUD);
    let mut si_reader = sportident::SiReader::new(transport);
    log::info!("SI master connected");

    loop {
        match radio.receive() {
            Ok(Some(pkt)) => {
                let text = core::str::from_utf8(&pkt.payload).unwrap_or("<non-utf8>");
                log::info!("RX from {:#06x} rssi={:?}: {}", pkt.src_addr, pkt.rssi, text);
            }
            Ok(None) => {}
            Err(e) => log::warn!("receive() error: {:?}", e),
        }

        match si_reader.read_event() {
            Ok(Some(sportident::SiEvent::CardReadout(readout))) => {
                log::info!("SI card {} ({} punches)", readout.card_id, readout.punches.len());
                let payload = readout.to_payload(current.addr);
                match radio.send(current.dest, payload.as_bytes()) {
                    Ok(()) => log::info!("TX -> {:#06x}: {}", current.dest, payload),
                    Err(e) => log::warn!("send() error: {:?}", e),
                }
            }
            Ok(Some(sportident::SiEvent::CardRemoved)) => {}
            Ok(None) => {}
            Err(e) => log::warn!("SI read error: {:?}", e),
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
