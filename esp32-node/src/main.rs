//! ESP32-S3 SportIdent punch relay node.
//!
//! No physical config switch: the node always boots into a 2-minute Wi-Fi
//! config portal first (see wifi_config.rs) — a technician can change
//! addr/dest/freq there without reflashing; if nothing is saved, Wi-Fi is
//! stopped and normal operation proceeds with whatever's in NVS (or these
//! defaults on first boot). Reads punches from the SI master over USB
//! (cp210x.rs + sportident.rs) and relays them to the base station over
//! LoRa, using the same wire format lora-server already parses. The pending-
//! punch queue is allocated in PSRAM (psram.rs), not the main heap.

// `Allocator` is nightly-only; the esp-rs Xtensa toolchain is itself a
// nightly build, so this is available — see psram.rs's own doc comment for
// what breaks if this ever moves to a stable compiler.
#![feature(allocator_api)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use embedded_hal::spi::MODE_0;
use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::PinDriver;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::prelude::*;
use esp_idf_hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver, SpiDriverConfig};
use esp_idf_svc::eventloop::EspSystemEventLoop;

use sx127x::{Bandwidth, CodingRate, Config as RadioConfig, LoraRadio, Sx127xSpi};

mod battery;
mod config;
mod cp210x;
mod persistent_log;
mod protocol;
mod psram;
mod sportident;
mod wifi_config;

use config::NodeConfig;
use psram::PsramAllocator;
use sportident::CardReadout;

/// How often an unacked punch is retried — matches lora-server's own
/// PUNCH_RETRY_INTERVAL exactly (not load-bearing for correctness, just
/// consistent with the rest of the fleet). No give-up count: a punch is
/// real event data, retried indefinitely rather than dropped, per
/// docs/protocols/lora_online_control_protocol.md's "Punch Delivery".
const PUNCH_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Matches lora-server's own --heartbeat-interval default. Sent from the
/// main thread, entirely independent of whether an SI master is connected
/// (see spawn_si_reader_thread) — a node with a dead/unplugged reader but a
/// healthy radio should still show up as alive to the base station, not go
/// silent just because wait_for_si_master is blocked.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

struct PendingPunch {
    card_id: u32,
    payload: String,
    sent_at: Instant,
    attempts: u32,
}

/// Owns the SI master connection end-to-end: (re)connecting, reading
/// punches, and noticing disconnects — entirely on its own thread, so a
/// missing/dead SI master never blocks the radio/heartbeat loop in main()
/// (see HEARTBEAT_INTERVAL's doc comment). Hands punches to the main thread
/// over `punch_tx` rather than touching the radio directly, mirroring
/// lora-server's own split between sportident.rs's hotplug thread and
/// run_daemon_loop's radio ownership. `si_present` is flipped false the
/// instant a connection is lost or not yet established, true only once the
/// SI master actually answers — main() reports this as-is in every
/// heartbeat rather than only while actively reading punches.
fn spawn_si_reader_thread(punch_tx: mpsc::Sender<CardReadout>, si_present: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("si-reader".into())
        .spawn(move || loop {
            log::info!("waiting for SI master (VID {:#06x} PID {:#06x})...", sportident::SI_VID, sportident::SI_PID);
            si_present.store(false, Ordering::SeqCst);
            let transport = cp210x::wait_for_si_master(sportident::SI_PID, sportident::SI_BAUD);
            si_present.store(true, Ordering::SeqCst);
            log::info!("SI master connected");
            let mut si_reader = sportident::SiReader::new(transport);

            loop {
                if si_reader.transport().is_disconnected() {
                    log::warn!("SI master disconnected — will reconnect");
                    si_present.store(false, Ordering::SeqCst);
                    break;
                }
                match si_reader.read_event() {
                    Ok(Some(sportident::SiEvent::CardReadout(readout))) => {
                        log::info!("read SI card {} ({} punches)", readout.card_id, readout.punches.len());
                        if punch_tx.send(readout).is_err() {
                            // Main thread is gone (panicked/exited) — nothing
                            // left to hand punches to.
                            return;
                        }
                    }
                    Ok(Some(sportident::SiEvent::CardRemoved)) => {}
                    Ok(None) => {}
                    Err(e) => log::warn!("SI read error: {:?}", e),
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .expect("failed to spawn SI reader thread");
}

/// `<semver>+<git-sha>[.dirty]`, e.g. "0.1.0+a1b2c3d4" or
/// "0.1.0+a1b2c3d4.dirty" — same scheme as lora-server's and roc-server's
/// own version.rs (no shared crate to hang one definition off of). SEMVER
/// comes from the repo-root VERSION file, GIT_SHA from `git describe`; both
/// embedded by build.rs. Logged on every boot below — the only way to tell
/// what firmware an unattended field node is actually running without
/// physically re-flashing it to find out.
const VERSION: &str = concat!(env!("SEMVER"), "+", env!("GIT_SHA"));

// Fixed modem parameters, shared fleet-wide — not exposed via the config
// page (only addr/dest/freq are; see wifi_config.rs). Must match
// lora-base-station's deployed /etc/lora-server/env.
const SPREADING_FACTOR: u8 = 7;
const BANDWIDTH: Bandwidth = Bandwidth::Khz125;
const CODING_RATE: CodingRate = CodingRate::Cr4_5;
const SYNC_WORD: u8 = 0x12;
const TX_POWER_DBM: i8 = 20;

/// First-boot defaults. addr=10 is this node's own address; dest=1 targets
/// lora-base-station directly (its LoRa address, not an IP — see
/// docs/protocols/lora_online_control_protocol.md). A function, not a
/// const, since NodeConfig::network_id is a heap String — String::from
/// isn't callable in a const context.
fn default_config() -> NodeConfig {
    NodeConfig { addr: 10, dest: 1, freq_hz: 433_000_000, network_id: "LOC".to_string() }
}

fn main() -> anyhow::Result<()> {
    // Required on every esp-idf-svc std binary before touching any ESP-IDF
    // API — links libc/newlib patches the IDF needs.
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // First line out on every boot, deliberately before anything else can
    // fail — an unattended field node rebooting on its own (brownout,
    // watchdog, panic) is otherwise invisible; this is the only way to tell
    // "power-cycled on purpose" apart from "crashed" after the fact from a
    // serial log.
    let reset_reason = esp_idf_hal::reset::ResetReason::get();
    log::info!("esp32-node {} booting (reset reason: {:?})", VERSION, reset_reason);

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    let nvs = Arc::new(Mutex::new(config::open_nvs()?));
    let current = {
        let guard = nvs.lock().unwrap();
        NodeConfig::load(&guard, default_config())
    };

    // First persistent-log checkpoint of this boot — see persistent_log.rs's
    // doc comment for why this exists at all: a crash later in this same
    // boot (brownout, panic, watchdog) means the live serial console is
    // long gone by the time it happens (cp210x::install(), below, steals
    // it), but this survives in NVS for the *next* boot's Wi-Fi portal
    // (/log) to show — telling you how far this boot actually got.
    persistent_log::append(&mut nvs.lock().unwrap(), &format!("boot: {:?}", reset_reason));

    // Either returns after the window closes with `current` still accurate
    // (nothing saved), or a save inside the portal calls esp_restart()
    // directly and this call never returns at all. Cloned since `current`
    // (not Copy — network_id is a String) is still needed below.
    wifi_config::run(peripherals.modem, sysloop, Arc::clone(&nvs), current.clone())?;

    let pins = peripherals.pins;

    // Pin assignment matches the schematic's suggested wiring (Note 1: not
    // fixed, adjust freely if your board's silkscreen numbering differs).
    let sclk = pins.gpio12;
    let sdo = pins.gpio11; // MOSI
    let sdi = pins.gpio13; // MISO
    let cs = pins.gpio10; // NSS
    let reset = PinDriver::output(pins.gpio9)?;
    // DIO0 (GPIO7 — GPIO14/15/16 are SMD probe points on this board, not
    // usable header pins, see the wiring doc) is now physically connected,
    // so completion (TX/CAD) is detected via this pin instead of polling
    // IRQ_FLAGS over SPI.
    let dio0 = PinDriver::input(pins.gpio7)?;

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
        "esp32-node up: addr={} dest={} freq={}Hz sf={} network_id={}",
        current.addr, current.dest, current.freq_hz, SPREADING_FACTOR, current.network_id
    );
    persistent_log::append(&mut nvs.lock().unwrap(), "radio up");

    // Announced once, unprompted, right after the radio is up — lets
    // lora-base-station learn a node's firmware version passively (see
    // daemon_state::NodeStatus::version) without an operator having to
    // remember to query every node after a redeploy. Best-effort like every
    // other uplink send here: if this one send is lost, the node's version
    // just won't show up until the next boot or an explicit /queryversion —
    // not worth retrying for a value that never changes mid-session.
    let boot_report = protocol::encode_version_report(current.addr, VERSION);
    match protocol::send_framed(&mut radio, current.dest, boot_report.as_bytes(), &current.network_id) {
        Ok(()) => {
            log::info!("VERSION to {:#06x}: {}", current.dest, boot_report);
            persistent_log::append(&mut nvs.lock().unwrap(), "boot announce: ok");
        }
        Err(e) => {
            log::warn!("boot version announcement failed: {:?}", e);
            persistent_log::append(&mut nvs.lock().unwrap(), &format!("boot announce: err {:?}", e));
        }
    }

    // GPIO4: placeholder battery-sense pin, see battery.rs and the wiring
    // doc — the actual voltage-divider circuit isn't built yet.
    let mut battery = battery::BatteryMonitor::new(peripherals.adc1, pins.gpio4)?;
    let mut last_heartbeat = Instant::now() - HEARTBEAT_INTERVAL; // send one immediately on boot
    // Included in each persisted heartbeat checkpoint below — lets the next
    // boot's /log page distinguish "died on the very first attempt" from
    // "ran fine for a while, then died", not just "died somewhere".
    let mut hb_attempt: u32 = 0;

    cp210x::install()?;

    // SI master connection lives entirely on its own thread now (see
    // spawn_si_reader_thread's doc comment) — this thread never blocks on
    // it, so radio RX/ack handling and heartbeats keep running even with no
    // reader plugged in at all.
    let (punch_tx, punch_rx) = mpsc::channel::<CardReadout>();
    let si_present = Arc::new(AtomicBool::new(false));
    spawn_si_reader_thread(punch_tx, Arc::clone(&si_present));

    // Punches read from the SI master land here first — actual transmission
    // (and its stop-and-wait retry) is driven by this queue below, mirroring
    // lora-server's own punch_buffer/PendingPunch design, minus persistence
    // across a reboot (see docs/protocols/lora_online_control_protocol.md's
    // "Punch Delivery"). Losing this queue on a crash/power cycle — unlike
    // the RPi's SQLite-backed buffer — is a known gap, not something worth
    // solving before this link is proven on real hardware.
    //
    // Allocated in PSRAM (see psram.rs), not the main heap — this queue is
    // the one place a backlog could actually grow (a burst of punches
    // arriving faster than the stop-and-wait ack lets them drain), so it's
    // the one worth pinning off the scarce internal RAM.
    let mut punch_queue: VecDeque<CardReadout, PsramAllocator> = VecDeque::new_in(PsramAllocator);
    let mut pending_punch: Option<PendingPunch> = None;

    loop {
        match protocol::receive_framed(&mut radio, &current.network_id) {
            Ok(Some((src_addr, rssi, text))) => {
                if let Some((node, card_id)) = protocol::parse_punch_ack(&text) {
                    if node == current.addr
                        && pending_punch.as_ref().is_some_and(|p| p.card_id == card_id)
                    {
                        log::info!("PUNCH card {} acked by {:#06x}", card_id, src_addr);
                        pending_punch = None;
                    }
                } else if let Some(target) = protocol::parse_version_query(&text) {
                    if target == current.addr {
                        let report = protocol::encode_version_report(current.addr, VERSION);
                        match protocol::send_framed(&mut radio, src_addr, report.as_bytes(), &current.network_id) {
                            Ok(()) => log::info!("VERSION to {:#06x}: {}", src_addr, report),
                            Err(e) => log::warn!("version query reply failed: {:?}", e),
                        }
                    }
                } else {
                    log::info!("RX from {:#06x} rssi={:?}: {}", src_addr, rssi, text);
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("receive() error: {:?}", e),
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            // "- -" (not bare tokens) keeps the field count fixed at three
            // whether or not the read succeeded, so lora-server's parser
            // doesn't need to guess which two of three fields are missing.
            let battery_field = match battery.read() {
                Ok((pct, mv)) => std::format!("{} {}", pct, mv),
                Err(e) => {
                    log::warn!("battery read failed ({:?}), reporting no battery data", e);
                    "- -".to_string()
                }
            };
            let si_flag = if si_present.load(Ordering::SeqCst) { '1' } else { '0' };
            let hb_payload = std::format!("HB {} {}", battery_field, si_flag);
            hb_attempt += 1;
            match protocol::send_framed(&mut radio, current.dest, hb_payload.as_bytes(), &current.network_id) {
                Ok(()) => {
                    log::info!("HB to {:#06x}: {}", current.dest, hb_payload);
                    persistent_log::append(&mut nvs.lock().unwrap(), &std::format!("hb #{}: ok", hb_attempt));
                }
                Err(e) => {
                    log::warn!("HB send failed: {:?}", e);
                    persistent_log::append(&mut nvs.lock().unwrap(), &std::format!("hb #{}: err {:?}", hb_attempt, e));
                }
            }
        }

        while let Ok(readout) = punch_rx.try_recv() {
            log::info!("buffered SI card {} ({} punches)", readout.card_id, readout.punches.len());
            punch_queue.push_back(readout);
        }

        // Stop-and-wait: only one punch outstanding at a time. The next
        // queued punch isn't even attempted until this one is acked.
        if pending_punch.is_none() {
            if let Some(readout) = punch_queue.pop_front() {
                let payload = readout.to_payload(current.addr, current.dest);
                match protocol::send_framed(&mut radio, current.dest, payload.as_bytes(), &current.network_id) {
                    Ok(()) => {
                        log::info!("PUNCH to {:#06x}: {}", current.dest, payload);
                        pending_punch = Some(PendingPunch {
                            card_id: readout.card_id, payload, sent_at: Instant::now(), attempts: 1,
                        });
                    }
                    Err(e) => {
                        log::warn!("PUNCH send failed ({:?}), will retry", e);
                        punch_queue.push_front(readout);
                    }
                }
            }
        } else if let Some(p) = &mut pending_punch {
            if p.sent_at.elapsed() >= PUNCH_RETRY_INTERVAL {
                p.attempts += 1;
                p.sent_at = Instant::now();
                match protocol::send_framed(&mut radio, current.dest, p.payload.as_bytes(), &current.network_id) {
                    Ok(()) => log::info!("PUNCH retry #{} to {:#06x}: {}", p.attempts, current.dest, p.payload),
                    Err(e) => log::warn!("PUNCH retry failed: {:?}", e),
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
