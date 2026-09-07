mod activity;
mod mip;
mod roc;
mod store;

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tiny_http::{Header, Method, Response, Server};

use activity::SharedActivity;
use store::Store;

#[derive(Parser, Debug)]
#[command(name = "roc-server", about = "MIP/ROC output server for buffered LoRa punches")]
struct Args {
    /// Address:port to listen on.
    #[arg(long, env = "ROC_SERVER_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,

    /// Path to this server's own SQLite punch log.
    #[arg(long, env = "ROC_SERVER_DB", default_value = "/var/lib/roc-server/punches.db")]
    db: String,

    /// lora-server's health-check URL (e.g. http://100.x.y.z:8081/health).
    /// If unset, this server doesn't check lora-server's reachability at all
    /// (it still serves its own /health regardless).
    #[arg(long, env = "ROC_SERVER_LORA_HEALTH_URL")]
    lora_health_url: Option<String>,

    /// How often to check lora-server's health endpoint, in seconds.
    #[arg(long, env = "ROC_SERVER_HEALTH_CHECK_INTERVAL_SECS", default_value_t = 30)]
    health_check_interval_secs: u64,
}

/// Mirrors lora-server's own health-check thread (lora-server/src/
/// health.rs) — see docs/protocols/lora_online_control_protocol.md,
/// "Network Identification" for the sibling design note on why this stays
/// a plain reachability check, not anything more elaborate. Logs only on
/// state changes so a healthy link doesn't spam the log every interval.
fn spawn_health_checker(lora_health_url: String, interval: std::time::Duration) {
    std::thread::Builder::new()
        .name("lora-health-check".into())
        .spawn(move || {
            let mut last_reachable: Option<bool> = None;
            loop {
                let reachable = ureq::get(&lora_health_url)
                    .timeout(std::time::Duration::from_secs(5))
                    .call()
                    .is_ok();

                if last_reachable != Some(reachable) {
                    if reachable {
                        log::info!("lora-server reachable at {}", lora_health_url);
                    } else {
                        log::warn!("lora-server unreachable at {}", lora_health_url);
                    }
                    last_reachable = Some(reachable);
                }

                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn lora-server health-check thread");
}

#[derive(Deserialize)]
struct PunchPush {
    card_id: u32,
    station: u8,
    time_s: u32,
    #[allow(dead_code)]
    source: String,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    let args = Args::parse();

    if let Some(parent) = std::path::Path::new(&args.db).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = Arc::new(Store::open(&args.db)?);
    let activity = activity::new_shared();

    if let Some(lora_health_url) = args.lora_health_url.clone() {
        spawn_health_checker(lora_health_url, std::time::Duration::from_secs(args.health_check_interval_secs));
    }

    log::info!("roc-server listening on {} (db {})", args.listen, args.db);
    let server = Server::http(&args.listen).map_err(|e| anyhow::anyhow!("{e}"))?;

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();

        let response = match (&method, path.as_str()) {
            (Method::Post, "/punches") => handle_punches(&mut request, &store, &activity),
            (Method::Get, "/mip") => handle_mip(&request, &url, &store, &activity),
            (Method::Get, "/roc") => handle_roc(&url, &store, &activity),
            (Method::Get, "/health") => text_response(200, "ok"),
            (Method::Get, "/") => handle_dashboard(&activity, &args.lora_health_url),
            (Method::Get, "/status.json") => handle_status_json(&activity, &args.lora_health_url),
            _ => text_response(404, "not found"),
        };

        if let Err(e) = request.respond(response) {
            log::warn!("failed to send response: {e}");
        }
    }

    Ok(())
}

fn text_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(status)
}

fn xml_response(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/xml; charset=utf-8"[..]).unwrap();
    Response::from_string(body).with_header(header)
}

fn handle_punches(request: &mut tiny_http::Request, store: &Store, activity: &SharedActivity) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        log::warn!("failed to read /punches body: {e}");
        return text_response(400, "bad request");
    }

    let punch: PunchPush = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("invalid /punches payload: {e}");
            return text_response(400, "invalid json");
        }
    };

    match store.record(punch.card_id, punch.station, punch.time_s, &punch.source) {
        Ok(id) => {
            log::info!("recorded punch id={id} card={} station={}", punch.card_id, punch.station);
            let mut a = activity.lock().unwrap();
            a.record_source(&punch.source);
            a.push_log(format!(
                "PUNCH id={id} card={} station={} source={}", punch.card_id, punch.station, punch.source
            ));
            text_response(200, "ok")
        }
        Err(e) => {
            log::error!("failed to record punch: {e}");
            text_response(500, "internal error")
        }
    }
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn last_id_from_request(request: &tiny_http::Request, url: &str) -> i64 {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("LastId"))
        .and_then(|h| h.value.as_str().parse().ok())
        .or_else(|| query_param(url, "lastid").and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn handle_mip(request: &tiny_http::Request, url: &str, store: &Store, activity: &SharedActivity) -> Response<std::io::Cursor<Vec<u8>>> {
    let last_id = last_id_from_request(request, url);
    match store.since(last_id) {
        Ok(punches) => {
            let new_last_id = punches.last().map(|p| p.id).unwrap_or(last_id);
            activity.lock().unwrap().push_log(format!("MIP poll lastid={last_id} -> {} punch(es)", punches.len()));
            xml_response(mip::render_mip_xml(new_last_id, &punches))
        }
        Err(e) => {
            log::error!("failed to query store for /mip: {e}");
            text_response(500, "internal error")
        }
    }
}

fn handle_roc(url: &str, store: &Store, activity: &SharedActivity) -> Response<std::io::Cursor<Vec<u8>>> {
    let last_id: i64 = query_param(url, "lastId").and_then(|v| v.parse().ok()).unwrap_or(0);
    match store.since(last_id) {
        Ok(punches) => {
            let mut timestamps = Vec::with_capacity(punches.len());
            for p in &punches {
                match store.timestamp_of(p.id) {
                    Ok(Some(ts)) => timestamps.push(ts),
                    Ok(None) => timestamps.push(String::new()),
                    Err(e) => {
                        log::error!("failed to look up timestamp for punch {}: {e}", p.id);
                        return text_response(500, "internal error");
                    }
                }
            }
            activity.lock().unwrap().push_log(format!("ROC poll lastId={last_id} -> {} punch(es)", punches.len()));
            text_response(200, &roc::render_roc_text(&punches, &timestamps))
        }
        Err(e) => {
            log::error!("failed to query store for /roc: {e}");
            text_response(500, "internal error")
        }
    }
}

#[derive(Serialize)]
struct SourceView {
    source: String,
    last_seen_secs_ago: Option<u64>,
    punch_count: u64,
}

#[derive(Serialize)]
struct DashboardView {
    lora_reachable: Option<bool>,
    sources: Vec<SourceView>,
    log: Vec<String>,
}

fn secs_ago(t: std::time::SystemTime) -> u64 {
    std::time::SystemTime::now().duration_since(t).unwrap_or_default().as_secs()
}

fn build_dashboard(activity: &SharedActivity, lora_health_url: &Option<String>) -> DashboardView {
    let guard = activity.lock().unwrap();
    let mut sources: Vec<SourceView> = guard
        .sources
        .iter()
        .map(|(source, s)| SourceView {
            source: source.clone(),
            last_seen_secs_ago: s.last_seen.map(secs_ago),
            punch_count: s.punch_count,
        })
        .collect();
    sources.sort_by(|a, b| a.source.cmp(&b.source));

    let log: Vec<String> = guard.log.iter().rev().map(|e| format!("[-{}s] {}", secs_ago(e.at), e.line)).collect();
    drop(guard);

    let lora_reachable = lora_health_url
        .as_ref()
        .map(|url| ureq::get(url).timeout(std::time::Duration::from_secs(2)).call().is_ok());

    DashboardView { lora_reachable, sources, log }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn handle_status_json(activity: &SharedActivity, lora_health_url: &Option<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    let v = build_dashboard(activity, lora_health_url);
    let body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    Response::from_string(body).with_header(header)
}

fn handle_dashboard(activity: &SharedActivity, lora_health_url: &Option<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    let v = build_dashboard(activity, lora_health_url);

    let lora_cell = match v.lora_reachable {
        Some(true) => "<td style=\"color:green\">reachable</td>".to_string(),
        Some(false) => "<td style=\"color:red\">unreachable</td>".to_string(),
        None => "<td>not checked (--lora-health-url unset)</td>".to_string(),
    };

    let mut source_rows = String::new();
    for s in &v.sources {
        source_rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            html_escape(&s.source),
            s.last_seen_secs_ago.map(|s| format!("{s}s ago")).unwrap_or_else(|| "-".to_string()),
            s.punch_count,
        ));
    }
    if source_rows.is_empty() {
        source_rows = "<tr><td colspan=\"3\">no punches received yet</td></tr>\n".to_string();
    }

    let mut log_lines = String::new();
    for line in &v.log {
        log_lines.push_str(&html_escape(line));
        log_lines.push('\n');
    }
    if log_lines.is_empty() {
        log_lines = "(empty)".to_string();
    }

    let body = format!(
        r#"<html><head><title>roc-server status</title></head><body>
<h1>roc-server status</h1>
<table border="1" cellpadding="4">
<tr><th>lora-server</th>{lora_cell}</tr>
</table>

<h2>Punch sources</h2>
<p>"local"/"test" are lora-server's own SI reader / TESTPUNCH command; a
numeric source is a remote node's LoRa address.</p>
<table border="1" cellpadding="4">
<tr><th>Source</th><th>Last seen</th><th>Punch count</th></tr>
{source_rows}
</table>

<h2>Activity log (newest first)</h2>
<pre>{log_lines}</pre>
</body></html>"#
    );

    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    Response::from_string(body).with_header(header)
}
