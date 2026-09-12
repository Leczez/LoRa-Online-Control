//! First-two-minutes Wi-Fi config portal. No physical switch: while enabled,
//! the node boots into an open AP + a settings page for CONFIG_WINDOW, so a
//! technician can change addr/dest/freq/LoRa mode without reflashing. Saving
//! persists to NVS and reboots immediately to apply the new settings; if
//! nothing is saved, the window closes on its own and Wi-Fi is stopped
//! before normal operation starts — this firmware never touches Bluetooth
//! at all, so there's nothing to shut down there.
//!
//! **Disabled by default** (behind the `wifi-config-portal` Cargo feature,
//! see Cargo.toml) — replaced as the default safety net by main.rs's
//! ack-based auto-revert-to-standard-config logic, which needs no operator
//! present and doesn't cost every boot a fixed 2-minute Wi-Fi window. Kept
//! compiling and working, not deleted, since it's still useful for a
//! deliberate one-off reconfiguration session and may end up reused by a
//! future LoRa-triggered settings mode (see the design doc's open items).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use embedded_svc::io::Write;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::{Configuration as HttpServerConfig, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::{EspNvs, NvsDefault};
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, BlockingWifi, Configuration, EspWifi};

use crate::config::NodeConfig;
use crate::persistent_log;

const CONFIG_WINDOW: Duration = Duration::from_secs(120);

/// Runs the config portal for CONFIG_WINDOW. Returns once the window has
/// expired with nothing saved (Wi-Fi already stopped by the time this
/// returns) — a save reboots the device directly and never returns here.
pub fn run(
    modem: Modem,
    sysloop: EspSystemEventLoop,
    nvs: Arc<Mutex<EspNvs<NvsDefault>>>,
    current: NodeConfig,
) -> anyhow::Result<()> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), None)?, sysloop)?;

    let ssid = format!("esp32-node-{}-config", current.addr);
    wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
        ssid: ssid.as_str().try_into().unwrap(),
        auth_method: AuthMethod::None,
        ..Default::default()
    }))?;
    wifi.start()?;
    wifi.wait_netif_up()?;

    // Don't assume the AP got ESP-IDF's textbook-default 192.168.4.1 — it
    // didn't on this board (came up on 192.168.71.1 instead, presumably an
    // esp-idf-svc/sdkconfig default that differs from the raw-C SDK
    // example), and a hardcoded IP here would send a technician to a
    // subnet their client was never given an address on.
    let ip = wifi.wifi().ap_netif().get_ip_info()?.ip;
    log::info!("config portal up: connect to Wi-Fi \"{}\", browse to http://{}/", ssid, ip);

    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    let render_current = current.clone();
    server.fn_handler::<anyhow::Error, _>("/", Method::Get, move |req| {
        let html = render_page(&render_current);
        let mut resp = req.into_ok_response()?;
        resp.write_all(html.as_bytes())?;
        Ok(())
    })?;

    // Surfaces the persistent checkpoint log (persistent_log.rs) — the one
    // way to see how far a *previous* boot got before a crash (brownout,
    // panic, watchdog) without a live serial connection, since this portal
    // runs fresh at the start of every boot, before cp210x::install() ever
    // touches USB. See persistent_log.rs's doc comment for the full
    // rationale and why it's only a handful of short checkpoint lines, not
    // a full serial-log mirror.
    let log_nvs = Arc::clone(&nvs);
    server.fn_handler::<anyhow::Error, _>("/log", Method::Get, move |req| {
        let lines = { persistent_log::read_lines(&log_nvs.lock().unwrap()) };
        let html = render_log_page(&lines);
        let mut resp = req.into_ok_response()?;
        resp.write_all(html.as_bytes())?;
        Ok(())
    })?;

    let save_nvs = Arc::clone(&nvs);
    server.fn_handler::<anyhow::Error, _>("/save", Method::Post, move |mut req| {
        let mut buf = [0u8; 256];
        let len = req.read(&mut buf)?;
        let body = std::str::from_utf8(&buf[..len]).unwrap_or("");
        let form = parse_form(body);

        let updated = NodeConfig {
            addr: form.get("addr").and_then(|v| v.parse().ok()).unwrap_or(current.addr),
            dest: form.get("dest").and_then(|v| v.parse().ok()).unwrap_or(current.dest),
            freq_hz: form.get("freq").and_then(|v| v.parse().ok()).unwrap_or(current.freq_hz),
            sync_word: form.get("sync_word").and_then(|v| v.parse().ok()).unwrap_or(current.sync_word),
            // sf accepts any 7-12 value the driver itself would (clamped
            // further downstream); bw_hz/cr are validated against the
            // driver's actual supported sets here, before saving, since an
            // out-of-range NVS value would otherwise silently fall back to
            // the 125kHz/4:5 default at every future boot with no
            // indication why the submitted value didn't "stick".
            sf: form.get("sf")
                .and_then(|v| v.parse().ok())
                .filter(|v| (7..=12).contains(v))
                .unwrap_or(current.sf),
            bw_hz: form.get("bw_hz")
                .and_then(|v| v.parse().ok())
                .filter(|v| sx127x::Bandwidth::from_hz(*v).is_some())
                .unwrap_or(current.bw_hz),
            cr: form.get("cr")
                .and_then(|v| v.parse().ok())
                .filter(|v| sx127x::CodingRate::from_denominator(*v).is_some())
                .unwrap_or(current.cr),
        };
        {
            let mut guard = save_nvs.lock().unwrap();
            updated.save(&mut guard)?;
        }
        log::info!(
            "config saved (addr={} dest={} freq={} sf={} bw={}Hz cr=4/{} sync_word={}), rebooting to apply",
            updated.addr, updated.dest, updated.freq_hz, updated.sf, updated.bw_hz, updated.cr, updated.sync_word
        );

        let mut resp = req.into_ok_response()?;
        resp.write_all(b"<html><body>Saved. Restarting...</body></html>")?;
        drop(resp);

        std::thread::sleep(Duration::from_millis(500));
        esp_idf_hal::reset::restart();
    })?;

    std::thread::sleep(CONFIG_WINDOW);

    drop(server);
    wifi.stop()?;
    log::info!("config window closed, Wi-Fi stopped — proceeding to normal operation");
    Ok(())
}

fn parse_form(body: &str) -> std::collections::HashMap<&str, &str> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect()
}

/// The 10 bandwidths `sx127x::Bandwidth` supports, in Hz, paired with a
/// short practicality label — surfaced so a `<select>` only ever offers
/// values the driver actually accepts (see `Bandwidth::from_hz`), instead
/// of a free-text field that could produce a value silently falling back to
/// the default at next boot. See docs/protocols/lora_online_control_protocol.md,
/// "RF Parameters", for why anything below ~20.8kHz risks the crystal drift
/// on this hardware exceeding the channel width outright.
const BANDWIDTH_OPTIONS: [(u32, &str); 10] = [
    (7_800, "7.8 kHz — not recommended, likely narrower than this hardware's crystal drift"),
    (10_400, "10.4 kHz — same crystal-drift risk as 7.8 kHz"),
    (15_600, "15.6 kHz — same crystal-drift risk as 7.8 kHz"),
    (20_800, "20.8 kHz — borderline; only with LOW_DATA_RATE benefits already maxed out"),
    (31_250, "31.25 kHz"),
    (41_700, "41.7 kHz"),
    (62_500, "62.5 kHz — extra range over 125kHz with comfortable drift margin"),
    (125_000, "125 kHz — default, recommended"),
    (250_000, "250 kHz"),
    (500_000, "500 kHz — shortest range, fastest/lowest airtime"),
];

/// The 4 coding rates `sx127x::CodingRate` supports, paired with a short
/// tradeoff label.
const CODING_RATE_OPTIONS: [(u8, &str); 4] = [
    (5, "4/5 — default, lowest airtime overhead"),
    (6, "4/6"),
    (7, "4/7"),
    (8, "4/8 — most forward error correction, highest airtime overhead"),
];

fn render_bandwidth_options(current_hz: u32) -> String {
    BANDWIDTH_OPTIONS
        .iter()
        .map(|(hz, label)| {
            let selected = if *hz == current_hz { " selected" } else { "" };
            format!(r#"<option value="{hz}"{selected}>{label}</option>"#)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_coding_rate_options(current_denom: u8) -> String {
    CODING_RATE_OPTIONS
        .iter()
        .map(|(denom, label)| {
            let selected = if *denom == current_denom { " selected" } else { "" };
            format!(r#"<option value="{denom}"{selected}>{label}</option>"#)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_page(cfg: &NodeConfig) -> String {
    format!(
        r#"<html><body>
<h1>ESP32 Node Config</h1>
<form method="POST" action="/save">
  <label>Node address <input type="number" name="addr" value="{addr}"></label><br>
  <label>Base/dest address <input type="number" name="dest" value="{dest}"></label><br>
  <label>Frequency (Hz) <input type="number" name="freq" value="{freq}"></label><br>
  <label>Sync word (0-255, decimal) <input type="number" name="sync_word" min="0" max="255" value="{sync_word}"></label><br>
  <label>Spreading factor (7-12) <input type="number" name="sf" min="7" max="12" value="{sf}"></label><br>
  <label>Bandwidth <select name="bw_hz">{bw_options}</select></label><br>
  <label>Coding rate <select name="cr">{cr_options}</select></label><br>
  <button type="submit">Save &amp; restart</button>
</form>
<p>Sync word is the only guard against cross-talk with another
event running this same firmware nearby — must match lora-server's own
--sync-word exactly.</p>
<p>Spreading factor, bandwidth, and coding rate must all match
lora-base-station's own --sf/--bw-hz/--cr exactly, or the two ends simply
never hear each other (no error shown on either side when they mismatch).
See docs/protocols/lora_online_control_protocol.md, "RF Parameters", for
the range/airtime tradeoffs behind each choice.</p>
<p><a href="/log">View boot/checkpoint log</a></p>
</body></html>"#,
        addr = cfg.addr,
        dest = cfg.dest,
        freq = cfg.freq_hz,
        sync_word = cfg.sync_word,
        sf = cfg.sf,
        bw_options = render_bandwidth_options(cfg.bw_hz),
        cr_options = render_coding_rate_options(cfg.cr),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Oldest-first, one per line — matches the order persistent_log::read_lines
/// returns them in, which matches the order they were checkpointed in.
fn render_log_page(lines: &[String]) -> String {
    let body = if lines.is_empty() {
        "(empty — nothing checkpointed yet)".to_string()
    } else {
        lines.iter().map(|l| html_escape(l)).collect::<Vec<_>>().join("\n")
    };
    format!(
        r#"<html><body>
<h1>Checkpoint log</h1>
<p>Persisted across reboots (see persistent_log.rs) — oldest first. Only a
handful of meaningful checkpoints, not a full serial-log mirror.</p>
<pre>{body}</pre>
<p><a href="/">Back to config</a></p>
</body></html>"#
    )
}
