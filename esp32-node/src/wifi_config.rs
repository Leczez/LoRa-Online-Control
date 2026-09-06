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
use esp_idf_svc::sys::esp_restart;
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, BlockingWifi, Configuration, EspWifi};

use crate::config::NodeConfig;

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

    log::info!("config portal up: connect to Wi-Fi \"{}\", browse to http://192.168.4.1/", ssid);

    let mut server = EspHttpServer::new(&HttpServerConfig::default())?;

    let render_current = current.clone();
    server.fn_handler::<anyhow::Error, _>("/", Method::Get, move |req| {
        let html = render_page(&render_current);
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
            network_id: form.get("network_id").map(|v| v.to_string()).unwrap_or_else(|| current.network_id.clone()),
        };
        {
            let mut guard = save_nvs.lock().unwrap();
            updated.save(&mut guard)?;
        }

        let mut resp = req.into_ok_response()?;
        resp.write_all(b"<html><body>Saved. Restarting...</body></html>")?;
        drop(resp);

        std::thread::sleep(Duration::from_millis(500));
        unsafe { esp_restart() };
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
  <label>Network ID <input type="text" name="network_id" value="{network_id}"></label><br>
  <button type="submit">Save &amp; restart</button>
</form>
</body></html>"#,
        addr = cfg.addr,
        dest = cfg.dest,
        freq = cfg.freq_hz,
        network_id = cfg.network_id,
    )
}
