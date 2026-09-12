//! First-two-minutes Wi-Fi config portal. No physical switch: the node
//! always boots into an open AP + a settings page for CONFIG_WINDOW, so a
//! technician can change addr/dest/freq without reflashing. Saving persists
//! to NVS and reboots immediately to apply the new settings; if nothing is
//! saved, the window closes on its own and Wi-Fi is stopped before normal
//! operation starts — this firmware never touches Bluetooth at all, so
//! there's nothing to shut down there.

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
        };
        {
            let mut guard = save_nvs.lock().unwrap();
            updated.save(&mut guard)?;
        }
        log::info!(
            "config saved (addr={} dest={} freq={} sync_word={}), rebooting to apply",
            updated.addr, updated.dest, updated.freq_hz, updated.sync_word
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

fn render_page(cfg: &NodeConfig) -> String {
    format!(
        r#"<html><body>
<h1>ESP32 Node Config</h1>
<form method="POST" action="/save">
  <label>Node address <input type="number" name="addr" value="{addr}"></label><br>
  <label>Base/dest address <input type="number" name="dest" value="{dest}"></label><br>
  <label>Frequency (Hz) <input type="number" name="freq" value="{freq}"></label><br>
  <label>Sync word (0-255, decimal) <input type="number" name="sync_word" min="0" max="255" value="{sync_word}"></label><br>
  <button type="submit">Save &amp; restart</button>
</form>
<p>Sync word is the only guard against cross-talk with another
event running this same firmware nearby — must match lora-server's own
--sync-word exactly.</p>
<p><a href="/log">View boot/checkpoint log</a></p>
</body></html>"#,
        addr = cfg.addr,
        dest = cfg.dest,
        freq = cfg.freq_hz,
        sync_word = cfg.sync_word,
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
