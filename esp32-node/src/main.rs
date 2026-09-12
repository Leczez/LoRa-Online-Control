//! ESP32-S3 SportIdent punch relay node.
//!
//! Normal operation starts immediately from whatever's in NVS (or the
//! defaults on first boot) — no boot-time Wi-Fi window by default. Instead,
//! the radio's current LoRa mode (freq/sf/bw_hz/cr/sync_word) has 30 seconds
//! after boot to earn a `ConfigAck` from the base station (see
//! `CONFIG_VERIFY_WINDOW` below and docs/protocols/lora_online_control_protocol.md,
//! "RF Parameters"); if none arrives, it's reverted to a known-good standard
//! mode *for the rest of this boot only* — NVS is untouched, so a power
//! cycle tries the original saved value again from scratch, rather than the
//! node quietly getting stuck on "standard" forever after one bad reading
//! (e.g. a stretch of interference right at boot). The old Wi-Fi config
//! portal (wifi_config.rs) that used to be the only way to change these
//! still exists, just disabled by default — see Cargo.toml's
//! `wifi-config-portal` feature — superseded by a LoRa-triggered settings
//! mode (`Setting::SettingsMode` in protocol.rs, and this file's Command
//! dispatch in the main loop below): the base station puts a node into
//! settings mode, stages any number of field changes, then exits — which
//! saves and reboots, running the same post-boot verification window a bad
//! value would otherwise need to survive. Reads punches from the SI master
//! over USB (cp210x.rs + sportident.rs) and relays them to the base station
//! over LoRa, using the same wire format lora-server already parses. The
//! pending-punch queue is allocated in PSRAM (psram.rs), not the main heap.
//!
//! Build with `--features debug-console` for a bench/debug variant that
//! skips SI-master reading entirely so the USB serial console stays live
//! for the whole session (normally lost partway through boot once
//! cp210x::install() switches the native USB port into host mode), and
//! relays the persisted checkpoint log over LoRa right after the radio
//! comes up, before anything that could hang — see Cargo.toml's
//! debug-console feature. Never flash that build to a real field node.
//!
//! DIO0 completion detection defaults to `new_with_dio0`'s plain GPIO
//! polling. `dio0_interrupt.rs` offers a real-hardware-interrupt
//! alternative (`dio0-interrupt` feature, opt-in) — a field test found it
//! completing sends in ~10ms at SF11 (real CAD+TX takes 280ms+), i.e.
//! reporting success without actually transmitting; see that feature's own
//! doc comment in Cargo.toml before re-enabling it.

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
#[cfg(feature = "wifi-config-portal")]
use esp_idf_svc::eventloop::EspSystemEventLoop;

use sx127x::{Bandwidth, CodingRate, Config as RadioConfig, LoraRadio, Sx127xSpi};

mod battery;
mod config;
// Unreachable under the debug-console feature (main() skips
// cp210x::install() entirely there) — see Cargo.toml's debug-console
// feature and main()'s own comment at that call site.
#[cfg_attr(feature = "debug-console", allow(dead_code))]
mod cp210x;
// Off by default — see its own doc comment and Cargo.toml's dio0-interrupt
// feature for why.
#[cfg(feature = "dio0-interrupt")]
mod dio0_interrupt;
mod persistent_log;
mod protocol;
mod psram;
mod sportident;
// Disabled by default — see its own doc comment and Cargo.toml's
// wifi-config-portal feature.
#[cfg(feature = "wifi-config-portal")]
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
/// silent just because wait_for_si_master is blocked. Live-changeable via a
/// `Setting::HeartbeatIntervalSecs` command (main()'s Command dispatch) —
/// not persisted, so a reboot resets it back to this default, same as
/// lora-server's own `heartbeat_period` (backend.rs).
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// How long a freshly booted node gives its current LoRa mode to earn a
/// `ConfigAck` from the base station before giving up and reverting to
/// `NodeConfig::standard_rf()` for the rest of this boot (see main()). 30s,
/// not some fraction of a second — LoRa drops packets routinely, and both
/// the outbound boot announcement and the inbound ack can each be lost
/// independently, so this needs enough margin for a few retries, not just
/// one round trip.
const CONFIG_VERIFY_WINDOW: Duration = Duration::from_secs(30);

/// How often the boot announcement is re-sent while still waiting on a
/// `ConfigAck` — several attempts across `CONFIG_VERIFY_WINDOW` rather than
/// one, so a single lost packet (in either direction) can't by itself cause
/// a false "this config doesn't work" conclusion and an unnecessary revert.
const CONFIG_VERIFY_RETRY_INTERVAL: Duration = Duration::from_secs(10);

struct PendingPunch {
    card_id: u32,
    payload: Vec<u8>,
    sent_at: Instant,
    attempts: u32,
}

// Wire tag byte + fixed length for the binary HB frame — must match
// lora-server's own backend.rs::TAG_HB/HB_LEN exactly. See sportident.rs's
// TAG_PUNCH doc comment for the full tag registry.
const TAG_HB: u8 = 0x03;
const HB_FLAG_BATTERY_VALID: u8 = 0b001;
const HB_FLAG_SI_KNOWN: u8 = 0b010;
const HB_FLAG_SI_CONNECTED: u8 = 0b100;

/// Binary wire format: `[tag:1][flags:1][battery_pct:1][battery_mv:2]`,
/// big-endian, always 5 bytes — must match lora-server's own
/// `encode_heartbeat`/`parse_heartbeat` exactly. See
/// docs/protocols/lora_online_control_protocol.md, "Wire Format".
fn encode_heartbeat(battery: Option<(u8, u16)>, si_present: Option<bool>) -> [u8; 5] {
    let mut flags = 0u8;
    let (pct, mv) = match battery {
        Some((pct, mv)) => {
            flags |= HB_FLAG_BATTERY_VALID;
            (pct, mv)
        }
        None => (0, 0),
    };
    if let Some(si) = si_present {
        flags |= HB_FLAG_SI_KNOWN;
        if si {
            flags |= HB_FLAG_SI_CONNECTED;
        }
    }
    let mv_bytes = mv.to_be_bytes();
    [TAG_HB, flags, pct, mv_bytes[0], mv_bytes[1]]
}

/// Owns the SI master connection end-to-end: (re)connecting, reading
/// punches, and noticing disconnects — entirely on its own thread, so a
/// missing/dead SI master never blocks the radio/heartbeat loop in main()
/// (see DEFAULT_HEARTBEAT_INTERVAL's doc comment). Hands punches to the main thread
/// over `punch_tx` rather than touching the radio directly, mirroring
/// lora-server's own split between sportident.rs's hotplug thread and
/// run_daemon_loop's radio ownership. `si_present` is flipped false the
/// instant a connection is lost or not yet established, true only once the
/// SI master actually answers — main() reports this as-is in every
/// heartbeat rather than only while actively reading punches.
///
/// Unreachable under the debug-console feature (main() never calls this
/// there) — see Cargo.toml's debug-console feature.
#[cfg_attr(feature = "debug-console", allow(dead_code))]
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

// TX power is the one modem parameter that stays outside NodeConfig/NVS
// rather than a persisted field — it's commandable live over LoRa via
// `Setting::TxPowerDbm` (see "Command Packets" in
// docs/protocols/lora_online_control_protocol.md and main()'s Command
// dispatch), same as heartbeat interval: applied immediately, not
// persisted, resets to this default on reboot. Unlike SF/BW/CR/freq/
// sync_word/addr/dest, which can strand a node if misapplied and so are
// gated behind settings mode instead (see Setting's doc comment in
// protocol.rs).
const DEFAULT_TX_POWER_DBM: i8 = 20;

/// First-boot defaults. addr=10 is this node's own address; dest=1 targets
/// lora-base-station directly (its LoRa address, not an IP — see
/// docs/protocols/lora_online_control_protocol.md). The LoRa mode fields
/// (freq/sync_word/sf/bw_hz/cr) are `NodeConfig::standard_rf()`'s values —
/// see its doc comment for why a brand new node and a node recovering from
/// a bad mode both land on exactly the same numbers.
fn default_config() -> NodeConfig {
    NodeConfig {
        addr: 10,
        dest: 1,
        freq_hz: config::STANDARD_FREQ_HZ,
        sync_word: config::STANDARD_SYNC_WORD,
        sf: config::STANDARD_SF,
        bw_hz: config::STANDARD_BW_HZ,
        cr: config::STANDARD_CR,
    }
}

/// Recovers from a send failure by forcing a real hardware reset and full
/// register re-init (`configure()` calls `hardware_reset()` first thing) —
/// cheap insurance against a specific, observed failure mode: a timed-out
/// send can leave the chip in a state that plain per-attempt register
/// rewrites (which every send already does) don't clear, so every following
/// attempt — even `channel_activity_detected()`'s own CAD check, before any
/// transmission is even attempted — keeps failing too, forever, until the
/// node is physically power-cycled. Nothing else here ever re-initializes
/// the chip once boot's own `radio.configure()` call succeeds, so without
/// this a single bad transmission was permanent for the rest of that boot.
/// Best-effort: if the reconfigure itself fails, just log it and let the
/// next scheduled send attempt (which will likely also fail) try again —
/// there's nothing more targeted to fall back to here.
fn recover_radio<R: LoraRadio>(radio: &mut R, radio_config: &RadioConfig)
where
    R::Error: std::fmt::Debug,
{
    log::warn!("attempting radio recovery: forcing hardware reset + reconfigure");
    if let Err(e) = radio.configure(radio_config) {
        log::error!("radio recovery reconfigure failed: {:?}", e);
    }
}

/// Applies one field to the staged config if (and only if) this node is
/// currently in settings mode — returns whether it did, which the caller
/// uses to decide whether to Ack (see main()'s Command dispatch). A
/// rejection here is a normal, expected outcome, not an error: it's the
/// signal a commander sees as "no ack" when it sends a settings-mode-gated
/// field to a node that was never actually put into settings mode first.
fn stage_setting(settings_mode: &mut Option<NodeConfig>, src_addr: u16, apply: impl FnOnce(&mut NodeConfig)) -> bool {
    match settings_mode {
        Some(pending) => {
            apply(pending);
            true
        }
        None => {
            log::warn!("rejecting settings-mode-gated setting from {:#06x} — node not in settings mode", src_addr);
            false
        }
    }
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

    let nvs = Arc::new(Mutex::new(config::open_nvs()?));
    // Mutable: the ack-based verification below can revert this to
    // `standard_rf()` mid-boot if the value loaded here goes unacknowledged
    // (see CONFIG_VERIFY_WINDOW).
    let mut current = {
        let guard = nvs.lock().unwrap();
        NodeConfig::load(&guard, default_config())
    };

    // First persistent-log checkpoint of this boot — see persistent_log.rs's
    // doc comment for why this exists at all: a crash later in this same
    // boot (brownout, panic, watchdog) means the live serial console is
    // long gone by the time it happens (cp210x::install(), below, steals
    // it), but this survives in NVS for the *next* boot's Wi-Fi portal
    // (/log, when the wifi-config-portal feature is enabled) to show —
    // telling you how far this boot actually got.
    persistent_log::append(&mut nvs.lock().unwrap(), &format!("boot: {:?}", reset_reason));

    #[cfg(feature = "wifi-config-portal")]
    {
        let sysloop = EspSystemEventLoop::take()?;
        // Either returns after the window closes with `current` still
        // accurate (nothing saved), or a save inside the portal calls
        // esp_restart() directly and this call never returns at all.
        // NodeConfig is Copy, so passing it here doesn't consume the
        // binding `current` is still needed below.
        wifi_config::run(peripherals.modem, sysloop, Arc::clone(&nvs), current)?;
    }

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

    // Three mutually exclusive DIO0 completion strategies, selected at
    // compile time (not runtime) since they produce genuinely different
    // Sx127xSpi types — see Cargo.toml's dio0-interrupt/dio0-none feature
    // doc comments for why the interrupt-backed one is opt-in, and why
    // dio0-none (bypassing DIO0 entirely) exists as a diagnostic fallback.
    #[cfg(feature = "dio0-interrupt")]
    let mut radio = {
        let waiter = dio0_interrupt::Dio0Interrupt::new(dio0)?;
        Sx127xSpi::new_with_dio0_waiter(spi, reset, Delay::new_default(), waiter)
    };
    #[cfg(feature = "dio0-none")]
    let mut radio = {
        drop(dio0);
        Sx127xSpi::new(spi, reset, Delay::new_default())
    };
    #[cfg(not(any(feature = "dio0-interrupt", feature = "dio0-none")))]
    let mut radio = Sx127xSpi::new_with_dio0(spi, reset, Delay::new_default(), dio0);

    // Live-changeable via Setting::TxPowerDbm (see main loop's Command
    // dispatch) — not persisted, resets to DEFAULT_TX_POWER_DBM on reboot.
    let mut tx_power_dbm: i8 = DEFAULT_TX_POWER_DBM;

    // current.bw_hz/cr fall back to the same 125kHz/4:5 defaults on an
    // unrecognized value (e.g. a NodeConfig saved by older firmware) — see
    // NodeConfig::bw_hz's doc comment for why this is a soft fallback here,
    // unlike lora-server's --bw-hz/--cr which just refuse to start.
    //
    // Mutable: rebuilt from `current` if the config-verification logic below
    // reverts to `standard_rf()`, so every `recover_radio(&mut radio,
    // &radio_config)` call site elsewhere in this function keeps using
    // whichever config is actually live, not the one boot started with.
    let mut radio_config = RadioConfig {
        freq_hz: current.freq_hz,
        addr: current.addr,
        spreading_factor: current.sf,
        bandwidth: Bandwidth::from_hz(current.bw_hz).unwrap_or(Bandwidth::Khz125),
        coding_rate: CodingRate::from_denominator(current.cr).unwrap_or(CodingRate::Cr4_5),
        sync_word: current.sync_word,
        tx_power_dbm,
        ..Default::default()
    };
    radio
        .configure(&radio_config)
        .map_err(|e| anyhow::anyhow!("radio configure failed: {:?}", e))?;

    log::info!(
        "esp32-node up: addr={} dest={} freq={}Hz sf={} bw={}Hz cr=4/{} sync_word={:#04x}",
        current.addr, current.dest, current.freq_hz, current.sf, current.bw_hz, current.cr, current.sync_word
    );
    persistent_log::append(&mut nvs.lock().unwrap(), "radio up");

    // Relays the persisted checkpoint log (persistent_log.rs — normally
    // only visible via the Wi-Fi portal's /log page, itself disabled by
    // default) over LoRa instead, as plain uplink text frames, right here —
    // as early as possible after the radio comes up, before anything else
    // this boot does that could hang or fail. The point: if a bug further
    // down (heartbeat send, config verification, ...) leaves this boot
    // stuck with no way to get a live serial console attached (e.g.
    // powering from a bench supply on a different machine than the one
    // watching logs), the checkpoint history — including how far the
    // *previous*, now-stuck boot got — has already gone out over the air
    // and shows up in lora-tui/the web dashboard regardless. Each line is
    // its own frame (not one combined payload) since the persisted history
    // can exceed a single LoRa packet's payload limit, and losing one frame
    // to a collision shouldn't take the rest with it. Gated behind
    // debug-console since normal field operation has no use for this
    // (extra boot-time airtime, and a technician isn't watching lora-tui
    // during ordinary deployment) — this is purely a bring-up/bench
    // debugging aid.
    #[cfg(feature = "debug-console")]
    {
        let lines = persistent_log::read_lines(&nvs.lock().unwrap());
        log::info!("debug-console: relaying {} persistent-log line(s) over LoRa", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let payload = format!("DBGLOG {}/{} {}", i + 1, lines.len(), line);
            match radio.send(current.dest, payload.as_bytes()) {
                Ok(()) => log::info!("DBGLOG to {:#06x}: {}", current.dest, payload),
                Err(e) => {
                    log::warn!("debug-console: failed to relay log line: {:?}", e);
                    recover_radio(&mut radio, &radio_config);
                }
            }
            // Purely to keep the dashboard/lora-tui log readable one line at
            // a time rather than as a burst — radio.send() already blocks
            // until each frame's airtime is actually done, so this isn't
            // needed for correctness, just legibility.
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // Announced once immediately, then re-announced every
    // CONFIG_VERIFY_RETRY_INTERVAL until either a ConfigAck arrives or
    // CONFIG_VERIFY_WINDOW elapses (see the main loop below) — this is now
    // also the node's "is my current LoRa mode actually reaching the base
    // station" probe, not just a passive version-reporting convenience, so
    // it can no longer stay a true one-shot fire-and-forget send the way it
    // was before that mattered.
    let boot_report = protocol::encode_version_report(current.addr, VERSION);
    match radio.send(current.dest, boot_report.as_bytes()) {
        Ok(()) => {
            log::info!("VERSION to {:#06x}: {}", current.dest, boot_report);
            persistent_log::append(&mut nvs.lock().unwrap(), "boot announce: ok");
        }
        Err(e) => {
            log::warn!("boot version announcement failed: {:?}", e);
            persistent_log::append(&mut nvs.lock().unwrap(), &format!("boot announce: err {:?}", e));
            recover_radio(&mut radio, &radio_config);
        }
    }

    // Config-verification state (see CONFIG_VERIFY_WINDOW's doc comment).
    // `config_pending` covers both "still waiting" and "already reverted" —
    // once false, the main loop below never touches this again for the rest
    // of the boot, whether that's because a ConfigAck actually arrived or
    // because giving up already happened.
    let config_verify_deadline = Instant::now() + CONFIG_VERIFY_WINDOW;
    let mut config_pending = true;
    let mut last_config_verify_announce = Instant::now();

    // GPIO4: placeholder battery-sense pin, see battery.rs and the wiring
    // doc — the actual voltage-divider circuit isn't built yet.
    let mut battery = battery::BatteryMonitor::new(peripherals.adc1, pins.gpio4)?;
    // Live-changeable via Setting::HeartbeatIntervalSecs (see main loop's
    // Command dispatch) — not persisted, resets to
    // DEFAULT_HEARTBEAT_INTERVAL on reboot.
    let mut heartbeat_interval = DEFAULT_HEARTBEAT_INTERVAL;
    let mut last_heartbeat = Instant::now() - heartbeat_interval; // send one immediately on boot
    // Settings mode (see main loop's Command dispatch and Setting's doc
    // comment in protocol.rs): `None` = normal operation; `Some(pending)` =
    // staging a config change, `pending` being a working copy of `current`
    // that settings-mode-gated Command frames mutate in place. Nothing here
    // touches the live radio until `Setting::SettingsMode(false)` saves
    // `pending` and reboots — so an in-progress settings-mode session can
    // never itself break the ongoing command exchange with the base
    // station, even if the fields being staged (sync word, SF, ...) would
    // otherwise be exactly the ones that could.
    let mut settings_mode: Option<config::NodeConfig> = None;
    // Included in each persisted heartbeat checkpoint below — lets the next
    // boot's /log page distinguish "died on the very first attempt" from
    // "ran fine for a while, then died", not just "died somewhere".
    let mut hb_attempt: u32 = 0;

    // SI master connection lives entirely on its own thread now (see
    // spawn_si_reader_thread's doc comment) — this thread never blocks on
    // it, so radio RX/ack handling and heartbeats keep running even with no
    // reader plugged in at all.
    let (punch_tx, punch_rx) = mpsc::channel::<CardReadout>();
    let si_present = Arc::new(AtomicBool::new(false));

    #[cfg(not(feature = "debug-console"))]
    {
        // cp210x::install() switches the native USB port into host mode —
        // see cp210x.rs and this project's chat history for why that steals
        // the serial console for the rest of the boot. The debug-console
        // feature (Cargo.toml) skips this (and SI reading) entirely so the
        // console survives instead, for exactly the debugging situation
        // this comment is attached to.
        cp210x::install()?;
        spawn_si_reader_thread(punch_tx, Arc::clone(&si_present));
    }
    #[cfg(feature = "debug-console")]
    {
        // No producer for punch_tx in this build — drop it explicitly
        // rather than leave it an unused binding; punch_rx.try_recv() in
        // the main loop below simply never yields anything, and
        // si_present stays false forever, same as a node with no reader
        // plugged in at all.
        drop(punch_tx);
        log::warn!("debug-console build: SI master reading is disabled so the USB console stays available — never flash this to a field node");
    }

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
        match protocol::receive_raw(&mut radio) {
            Ok(Some((src_addr, rssi, bytes))) => {
                // PACK is binary (see protocol::parse_punch_ack) — checked
                // directly against the raw bytes before any UTF-8
                // conversion, since it's arbitrary binary and a lossy
                // decode would corrupt it. Only once that doesn't match do
                // we assume text, for the remaining (rare) control frames
                // this node understands (VQUERY/CACK).
                if let Some((node, card_id)) = protocol::parse_punch_ack(&bytes) {
                    log::info!("RX from {:#06x} rssi={:?}: PACK node={} card={}", src_addr, rssi, node, card_id);
                    if node == current.addr
                        && pending_punch.as_ref().is_some_and(|p| p.card_id == card_id)
                    {
                        log::info!("PUNCH card {} acked by {:#06x}", card_id, src_addr);
                        pending_punch = None;
                    }
                } else if let Ok(text) = core::str::from_utf8(&bytes) {
                    if let Some(target) = protocol::parse_version_query(text) {
                        if target == current.addr {
                            let report = protocol::encode_version_report(current.addr, VERSION);
                            match radio.send(src_addr, report.as_bytes()) {
                                Ok(()) => log::info!("VERSION to {:#06x}: {}", src_addr, report),
                                Err(e) => {
                                    log::warn!("version query reply failed: {:?}", e);
                                    recover_radio(&mut radio, &radio_config);
                                }
                            }
                        }
                    } else if let Some(target) = protocol::parse_config_ack(text) {
                        if target == current.addr && config_pending {
                            log::info!("current LoRa mode acked by {:#06x} — keeping it", src_addr);
                            persistent_log::append(&mut nvs.lock().unwrap(), "config verified");
                            config_pending = false;
                        }
                    } else if let Some((target, commander, setting)) = protocol::parse_command(text) {
                        if target == current.addr {
                            use protocol::Setting;
                            log::info!("CMD from {:#06x}: {:?}", src_addr, setting);
                            let applied = match setting {
                                Setting::HeartbeatIntervalSecs(secs) => {
                                    heartbeat_interval = Duration::from_secs(secs as u64);
                                    log::info!("heartbeat interval changed to {}s by command from {:#06x}", secs, src_addr);
                                    true
                                }
                                Setting::TxPowerDbm(dbm) => {
                                    tx_power_dbm = dbm;
                                    radio_config.tx_power_dbm = tx_power_dbm;
                                    recover_radio(&mut radio, &radio_config);
                                    log::info!("TX power changed to {}dBm by command from {:#06x}", dbm, src_addr);
                                    true
                                }
                                Setting::SettingsMode(true) => {
                                    settings_mode = Some(current);
                                    log::warn!("entered settings mode (command from {:#06x})", src_addr);
                                    persistent_log::append(&mut nvs.lock().unwrap(), "entered settings mode");
                                    true
                                }
                                Setting::SettingsMode(false) => match settings_mode.take() {
                                    Some(pending) => {
                                        if let Err(e) = pending.save(&mut nvs.lock().unwrap()) {
                                            log::error!("failed to save staged config: {:?}", e);
                                        }
                                        // Ack only after the save actually
                                        // happened, same "acknowledge
                                        // confirms applied" contract as
                                        // every other setting — radio.send()
                                        // is synchronous (blocks until TX
                                        // actually completes), so the ack is
                                        // already on air by the time this
                                        // returns; no artificial pre-reboot
                                        // delay needed the way the Wi-Fi
                                        // portal needs one for its HTTP
                                        // response to flush over TCP first.
                                        let ack = protocol::encode_ack(current.addr, commander, setting);
                                        let _ = radio.send(src_addr, ack.as_bytes());
                                        log::warn!("exiting settings mode, rebooting to apply new config");
                                        persistent_log::append(&mut nvs.lock().unwrap(), "settings mode: config saved, rebooting");
                                        esp_idf_hal::reset::restart();
                                    }
                                    None => {
                                        log::warn!("settings_mode=0 received but node wasn't in settings mode — ignoring");
                                        false
                                    }
                                },
                                // Settings-mode-gated fields: staged into
                                // the pending copy, never applied to the
                                // live radio here — only committed (and
                                // only then reconfigured) via
                                // SettingsMode(false) above. Only accepted
                                // (and acked) while actually in settings
                                // mode with a valid value — see Setting's
                                // doc comment in protocol.rs for why these
                                // specifically can't apply live the way
                                // HeartbeatIntervalSecs/TxPowerDbm do.
                                Setting::Addr(v) => stage_setting(&mut settings_mode, src_addr, |c| c.addr = v),
                                Setting::Dest(v) => stage_setting(&mut settings_mode, src_addr, |c| c.dest = v),
                                Setting::FreqHz(v) => stage_setting(&mut settings_mode, src_addr, |c| c.freq_hz = v),
                                Setting::SyncWord(v) => stage_setting(&mut settings_mode, src_addr, |c| c.sync_word = v),
                                Setting::Sf(v) if (7..=12).contains(&v) => {
                                    stage_setting(&mut settings_mode, src_addr, |c| c.sf = v)
                                }
                                Setting::BwHz(v) if Bandwidth::from_hz(v).is_some() => {
                                    stage_setting(&mut settings_mode, src_addr, |c| c.bw_hz = v)
                                }
                                Setting::Cr(v) if CodingRate::from_denominator(v).is_some() => {
                                    stage_setting(&mut settings_mode, src_addr, |c| c.cr = v)
                                }
                                _ => {
                                    log::warn!("rejecting {:?} from {:#06x} — invalid value or not in settings mode", setting, src_addr);
                                    false
                                }
                            };
                            if applied {
                                let ack = protocol::encode_ack(current.addr, commander, setting);
                                match radio.send(src_addr, ack.as_bytes()) {
                                    Ok(()) => log::info!("ACK to {:#06x}: {}", src_addr, ack),
                                    Err(e) => {
                                        log::warn!("failed to ack command: {:?}", e);
                                        recover_radio(&mut radio, &radio_config);
                                    }
                                }
                            }
                        }
                    } else {
                        log::info!("RX from {:#06x} rssi={:?}: {}", src_addr, rssi, text);
                    }
                } else {
                    log::info!("RX from {:#06x} rssi={:?}: <{} bytes, undecodable>", src_addr, rssi, bytes.len());
                }
            }
            Ok(None) => {}
            Err(e) => log::warn!("receive() error: {:?}", e),
        }

        if config_pending {
            if Instant::now() >= config_verify_deadline {
                // No ConfigAck within CONFIG_VERIFY_WINDOW despite retries —
                // the current LoRa mode isn't reaching the base station (or
                // its ack isn't reaching us; either way, this node can't be
                // trusted to stay reachable on it). addr/dest are untouched
                // (see standard_rf()'s doc comment) — only the modem
                // parameters revert.
                //
                // Deliberately NOT saved to NVS — this is a this-boot-only
                // fallback, not a permanent correction. A power cycle loads
                // the original (possibly-fine, possibly-still-bad) value
                // from NVS again and re-runs this same check from scratch,
                // rather than a single bad reading (e.g. a burst of
                // interference right at boot) quietly locking the node onto
                // "standard" forever.
                log::warn!(
                    "no config ack within {}s, reverting LoRa mode to standard for this boot",
                    CONFIG_VERIFY_WINDOW.as_secs()
                );
                persistent_log::append(&mut nvs.lock().unwrap(), "config unverified: reverted to standard (this boot only)");
                current = current.standard_rf();
                radio_config = RadioConfig {
                    freq_hz: current.freq_hz,
                    addr: current.addr,
                    spreading_factor: current.sf,
                    bandwidth: Bandwidth::from_hz(current.bw_hz).unwrap_or(Bandwidth::Khz125),
                    coding_rate: CodingRate::from_denominator(current.cr).unwrap_or(CodingRate::Cr4_5),
                    sync_word: current.sync_word,
                    tx_power_dbm,
                    ..Default::default()
                };
                recover_radio(&mut radio, &radio_config);
                config_pending = false;
            } else if last_config_verify_announce.elapsed() >= CONFIG_VERIFY_RETRY_INTERVAL {
                // Still within the window: give the current config another
                // chance to be heard rather than waiting on the one boot
                // announcement already sent — a single lost packet (either
                // direction) shouldn't by itself trigger a revert.
                last_config_verify_announce = Instant::now();
                let boot_report = protocol::encode_version_report(current.addr, VERSION);
                match radio.send(current.dest, boot_report.as_bytes()) {
                    Ok(()) => log::info!("VERSION (config-verify retry) to {:#06x}: {}", current.dest, boot_report),
                    Err(e) => {
                        log::warn!("config-verify re-announcement failed: {:?}", e);
                        recover_radio(&mut radio, &radio_config);
                    }
                }
            }
        }

        if last_heartbeat.elapsed() >= heartbeat_interval {
            last_heartbeat = Instant::now();
            let battery_reading = match battery.read() {
                Ok((pct, mv)) => Some((pct, mv)),
                Err(e) => {
                    log::warn!("battery read failed ({:?}), reporting no battery data", e);
                    None
                }
            };
            let si = si_present.load(Ordering::SeqCst);
            let hb_payload = encode_heartbeat(battery_reading, Some(si));
            hb_attempt += 1;
            match radio.send(current.dest, &hb_payload) {
                Ok(()) => {
                    log::info!(
                        "HB to {:#06x}: battery={} si={}",
                        current.dest,
                        battery_reading.map(|(pct, mv)| std::format!("{}% {}mV", pct, mv)).unwrap_or_else(|| "none".to_string()),
                        si
                    );
                    persistent_log::append(&mut nvs.lock().unwrap(), &std::format!("hb #{}: ok", hb_attempt));
                }
                Err(e) => {
                    log::warn!("HB send failed: {:?}", e);
                    persistent_log::append(&mut nvs.lock().unwrap(), &std::format!("hb #{}: err {:?}", hb_attempt, e));
                    recover_radio(&mut radio, &radio_config);
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
                match radio.send(current.dest, &payload) {
                    Ok(()) => {
                        log::info!(
                            "PUNCH to {:#06x}: card={} punches={}",
                            current.dest, readout.card_id, readout.punches.len()
                        );
                        pending_punch = Some(PendingPunch {
                            card_id: readout.card_id, payload, sent_at: Instant::now(), attempts: 1,
                        });
                    }
                    Err(e) => {
                        log::warn!("PUNCH send failed ({:?}), will retry", e);
                        punch_queue.push_front(readout);
                        recover_radio(&mut radio, &radio_config);
                    }
                }
            }
        } else if let Some(p) = &mut pending_punch {
            if p.sent_at.elapsed() >= PUNCH_RETRY_INTERVAL {
                p.attempts += 1;
                p.sent_at = Instant::now();
                match radio.send(current.dest, &p.payload) {
                    Ok(()) => log::info!("PUNCH retry #{} to {:#06x}: card={}", p.attempts, current.dest, p.card_id),
                    Err(e) => {
                        log::warn!("PUNCH retry failed: {:?}", e);
                        recover_radio(&mut radio, &radio_config);
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
