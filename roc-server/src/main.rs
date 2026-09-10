mod activity;
mod mip;
mod roc;
mod store;
mod version;

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

    /// Expected competition ID. MEOS sends one on every /mip (a "competition"
    /// header) and /roc (a "unitId" query param) request — verified against
    /// the real MEOS source (onlineinput.cpp's OnlineInput::process): both
    /// protocols send it, this server just never checked it before. Unset
    /// by default (`None`): every request is accepted regardless of what
    /// competition ID it names, which is exactly today's behavior and the
    /// only sane default for a single-event deployment — this server has no
    /// concept of "competitions" to scope by, and it's what every existing
    /// deployment already assumes. Set this only if MEOS is configured with
    /// a matching ID and you want /mip and /roc to reject requests that
    /// don't name it (e.g. a stray or misconfigured second MEOS instance
    /// pointed at the same server).
    #[arg(long, env = "ROC_SERVER_COMPETITION_ID")]
    competition_id: Option<String>,
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
    log::info!("roc-server {} starting", version::VERSION);

    if let Some(parent) = std::path::Path::new(&args.db).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = Arc::new(Store::open(&args.db)?);
    let activity = activity::new_shared();

    if let Some(lora_health_url) = args.lora_health_url.clone() {
        spawn_health_checker(lora_health_url, std::time::Duration::from_secs(args.health_check_interval_secs));
    }

    run_server(&args.listen, store, activity, args.lora_health_url, args.competition_id)
}

/// The actual request-serving loop, pulled out of main() so real end-to-end
/// tests can spawn it on a background thread and make genuine HTTP requests
/// against it — same pattern as lora-server/src/web.rs::spawn_server, which
/// this project leans on throughout rather than testing handlers against
/// hand-built request/response values.
fn run_server(
    listen: &str, store: Arc<Store>, activity: SharedActivity,
    lora_health_url: Option<String>, competition_id: Option<String>,
) -> Result<()> {
    log::info!("roc-server listening on {}", listen);
    let server = Server::http(listen).map_err(|e| anyhow::anyhow!("{e}"))?;

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();

        let response = match (&method, path.as_str()) {
            (Method::Post, "/punches") => handle_punches(&mut request, &store, &activity),
            (Method::Get, "/mip") => handle_mip(&request, &url, &store, &activity, &competition_id),
            (Method::Get, "/roc") => handle_roc(&url, &store, &activity, &competition_id),
            (Method::Get, "/health") => text_response(200, "ok"),
            (Method::Get, "/") => handle_dashboard(&activity, &lora_health_url),
            (Method::Get, "/status.json") => handle_status_json(&activity, &lora_health_url),
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

/// MEOS sends this as a `competition` header on every /mip request
/// (`key.emplace_back(L"competition", sanitizeId(cmpId));` in
/// OnlineInput::process, onlineinput.cpp) — confirmed against the real
/// source, not assumed.
fn competition_id_from_mip_request(request: &tiny_http::Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("competition"))
        .map(|h| h.value.as_str().to_string())
}

/// MEOS sends this as a `unitId` query param on every /roc request
/// (`q = L"?unitId=" + ... ` in the same function) — same source, same
/// verification.
fn competition_id_from_roc_request(url: &str) -> Option<String> {
    query_param(url, "unitId")
}

/// `expected` unset (`None`) means "don't check" — every request is
/// accepted regardless of what competition ID it names, or whether it
/// names one at all. That's the default and matches every existing
/// deployment's actual behavior; see Args::competition_id's doc comment.
fn competition_id_ok(expected: &Option<String>, actual: Option<&str>) -> bool {
    match expected {
        None => true,
        Some(want) => actual == Some(want.as_str()),
    }
}

fn handle_mip(
    request: &tiny_http::Request, url: &str, store: &Store, activity: &SharedActivity,
    expected_competition_id: &Option<String>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let actual_competition_id = competition_id_from_mip_request(request);
    if !competition_id_ok(expected_competition_id, actual_competition_id.as_deref()) {
        log::warn!("/mip request rejected: competition id {:?} doesn't match", actual_competition_id);
        activity.lock().unwrap().push_log(format!("MIP poll rejected: competition id {:?}", actual_competition_id));
        return text_response(403, "wrong competition id");
    }
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

fn handle_roc(
    url: &str, store: &Store, activity: &SharedActivity, expected_competition_id: &Option<String>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let actual_competition_id = competition_id_from_roc_request(url);
    if !competition_id_ok(expected_competition_id, actual_competition_id.as_deref()) {
        log::warn!("/roc request rejected: competition id {:?} doesn't match", actual_competition_id);
        activity.lock().unwrap().push_log(format!("ROC poll rejected: competition id {:?}", actual_competition_id));
        return text_response(403, "wrong competition id");
    }
    let last_id: i64 = query_param(url, "lastId").and_then(|v| v.parse().ok()).unwrap_or(0);
    match store.since(last_id) {
        Ok(punches) => {
            let date = match store.today() {
                Ok(d) => d,
                Err(e) => {
                    log::error!("failed to get today's date for /roc: {e}");
                    return text_response(500, "internal error");
                }
            };
            activity.lock().unwrap().push_log(format!("ROC poll lastId={last_id} -> {} punch(es)", punches.len()));
            text_response(200, &roc::render_roc_text(&punches, &date))
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
    /// `<semver>+<git-sha>[.dirty]` — see version.rs. Lets an operator (or a
    /// deploy script) confirm a redeploy actually took by comparing this
    /// against the commit they just pushed, without SSHing in.
    version: &'static str,
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

    DashboardView { version: version::VERSION, lora_reachable, sources, log }
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

/// Identical to lora-server/src/web.rs's own copy of this constant — see
/// its doc comment for why format!'s brace-escaping rules mean this has to
/// stay a separate constant rather than live inline in the template below.
/// No shared crate between the two binaries to hang one definition off of.
const DASHBOARD_STYLE: &str = r#"<style>
:root {
  --bg: #f5f6f8; --card: #ffffff; --border: #e3e6eb;
  --text: #1a1d23; --dim: #6b7280;
  --accent: #2563eb; --good: #16a34a; --bad: #dc2626; --warn: #b45309;
  --good-bg: #dcfce7; --bad-bg: #fee2e2; --warn-bg: #fef3c7;
}
* { box-sizing: border-box; }
body {
  margin: 0; padding: 2.5rem 1.5rem 4rem; background: var(--bg); color: var(--text);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  line-height: 1.5;
}
.wrap { max-width: 880px; margin: 0 auto; }
h1 { font-size: 1.5rem; margin: 0 0 .2rem; letter-spacing: -0.01em; }
.subtitle { color: var(--dim); font-size: .875rem; margin: 0 0 2rem; }
h2 {
  font-size: .78rem; text-transform: uppercase; letter-spacing: .06em;
  color: var(--dim); margin: 2rem 0 .6rem; font-weight: 600;
}
h2 .note { text-transform: none; letter-spacing: 0; font-weight: 400; }
.card {
  background: var(--card); border: 1px solid var(--border); border-radius: 10px;
  padding: 1rem 1.25rem; box-shadow: 0 1px 2px rgba(16, 24, 40, .04);
}
table { width: 100%; border-collapse: collapse; font-size: .88rem; }
th, td { text-align: left; padding: .5rem .6rem; border-bottom: 1px solid var(--border); }
th {
  font-size: .7rem; text-transform: uppercase; letter-spacing: .04em;
  color: var(--dim); font-weight: 600; white-space: nowrap;
}
tr:last-child td { border-bottom: none; }
td.mono, .mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
.pill {
  display: inline-block; padding: .15rem .55rem; border-radius: 999px;
  font-size: .76rem; font-weight: 600; white-space: nowrap;
}
.pill-good { background: var(--good-bg); color: var(--good); }
.pill-bad { background: var(--bad-bg); color: var(--bad); }
.pill-warn { background: var(--warn-bg); color: var(--warn); }
p.hint { color: var(--dim); font-size: .85rem; margin: 0 0 1rem; }
pre.log {
  background: #0b1020; color: #d7dde8; padding: 1rem; border-radius: 10px;
  font-size: .76rem; line-height: 1.6; overflow: auto; max-height: 440px; margin: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
</style>"#;

fn pill(good: bool, text: &str) -> String {
    format!("<span class=\"pill {}\">{}</span>", if good { "pill-good" } else { "pill-bad" }, text)
}

fn handle_dashboard(activity: &SharedActivity, lora_health_url: &Option<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    let v = build_dashboard(activity, lora_health_url);
    let version = html_escape(v.version);

    let lora_cell = match v.lora_reachable {
        Some(true) => pill(true, "reachable"),
        Some(false) => pill(false, "unreachable"),
        None => "<span class=\"pill pill-warn\">not checked (--lora-health-url unset)</span>".to_string(),
    };

    let mut source_rows = String::new();
    for s in &v.sources {
        source_rows.push_str(&format!(
            "<tr><td class=\"mono\">{}</td><td>{}</td><td class=\"mono\">{}</td></tr>\n",
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
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="10">
<title>roc-server status</title>
{DASHBOARD_STYLE}
</head>
<body>
<div class="wrap">
<h1>roc-server</h1>
<p class="subtitle">MIP/ROC output server status — <span class="mono">{version}</span></p>

<h2>Status</h2>
<div class="card">
<table>
<tr><th>lora-server</th><td>{lora_cell}</td></tr>
</table>
</div>

<h2>Punch sources</h2>
<div class="card">
<p class="hint">"local"/"test" are lora-server's own SI reader / TESTPUNCH command; a
numeric source is a remote node's LoRa address.</p>
<table>
<tr><th>Source</th><th>Last seen</th><th>Punch count</th></tr>
{source_rows}
</table>
</div>

<h2>Activity log <span class="note">(newest first, auto-refreshes)</span></h2>
<pre class="log">{log_lines}</pre>
</div>
</body>
</html>"#
    );

    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    Response::from_string(body).with_header(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_competition_id_ok_when_unconfigured_accepts_anything() {
        assert!(competition_id_ok(&None, None));
        assert!(competition_id_ok(&None, Some("anything")));
    }

    #[test]
    fn test_competition_id_ok_requires_exact_match_when_configured() {
        let expected = Some("evt-42".to_string());
        assert!(competition_id_ok(&expected, Some("evt-42")));
        assert!(!competition_id_ok(&expected, Some("evt-43")));
        assert!(!competition_id_ok(&expected, None));
    }

    #[test]
    fn test_competition_id_from_roc_request_reads_unit_id_query_param() {
        assert_eq!(competition_id_from_roc_request("/roc?unitId=evt-42&lastId=0"), Some("evt-42".to_string()));
        assert_eq!(competition_id_from_roc_request("/roc?lastId=0"), None);
    }

    fn free_listen_addr() -> String {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("127.0.0.1:{port}")
    }

    fn wait_for_server(base_url: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if ureq::get(&format!("{base_url}/health")).call().is_ok() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("server at {base_url} never came up");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Real run_server, real HTTP requests — not just competition_id_ok in
    /// isolation. Confirms a mismatched competition id gets 403'd on both
    /// /mip (header-based) and /roc (query-param-based), and that the exact
    /// same requests succeed once no competition id is configured at all
    /// (today's behavior, and the default).
    #[test]
    fn test_endpoints_reject_wrong_competition_id_when_configured() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store.record(123456, 31, 100, "local").unwrap();
        let activity = activity::new_shared();
        let listen = free_listen_addr();
        let listen_for_thread = listen.clone();
        let store_for_thread = Arc::clone(&store);
        let activity_for_thread = Arc::clone(&activity);

        std::thread::spawn(move || {
            let _ = run_server(
                &listen_for_thread, store_for_thread, activity_for_thread, None, Some("evt-42".to_string()),
            );
        });
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        let mip_wrong = ureq::get(&format!("{base_url}/mip"))
            .set("competition", "evt-99")
            .call();
        assert_eq!(mip_wrong.unwrap_err().into_response().unwrap().status(), 403);

        let mip_right = ureq::get(&format!("{base_url}/mip"))
            .set("competition", "evt-42")
            .call();
        assert_eq!(mip_right.unwrap().status(), 200);

        let roc_wrong = ureq::get(&format!("{base_url}/roc?unitId=evt-99&lastId=0")).call();
        assert_eq!(roc_wrong.unwrap_err().into_response().unwrap().status(), 403);

        let roc_right = ureq::get(&format!("{base_url}/roc?unitId=evt-42&lastId=0")).call();
        assert_eq!(roc_right.unwrap().status(), 200);
    }

    /// Same real server, but with no competition_id configured at all —
    /// every request must succeed regardless of what (if anything) it
    /// sends, since this is the default for every existing single-event
    /// deployment.
    #[test]
    fn test_endpoints_accept_any_competition_id_when_unconfigured() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let activity = activity::new_shared();
        let listen = free_listen_addr();
        let listen_for_thread = listen.clone();
        let store_for_thread = Arc::clone(&store);
        let activity_for_thread = Arc::clone(&activity);

        std::thread::spawn(move || {
            let _ = run_server(&listen_for_thread, store_for_thread, activity_for_thread, None, None);
        });
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        assert_eq!(ureq::get(&format!("{base_url}/mip")).call().unwrap().status(), 200);
        assert_eq!(
            ureq::get(&format!("{base_url}/mip")).set("competition", "whatever").call().unwrap().status(),
            200
        );
        assert_eq!(ureq::get(&format!("{base_url}/roc?lastId=0")).call().unwrap().status(), 200);
        assert_eq!(
            ureq::get(&format!("{base_url}/roc?unitId=whatever&lastId=0")).call().unwrap().status(),
            200
        );
    }

    /// Real handle_roc against a real store: confirms /roc's timestamp
    /// column reflects the punch's own time_s (10h01m10s for 36070s), not
    /// whatever wall-clock time the row happened to be inserted at — the
    /// actual bug this fix addresses, reported live as MIP and ROC
    /// disagreeing on a punch's time in MEOS.
    #[test]
    fn test_roc_endpoint_reports_time_of_day_from_punch_time_s_not_receipt_time() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        store.record(123456, 31, 36070, "local").unwrap();
        let activity = activity::new_shared();
        let listen = free_listen_addr();
        let listen_for_thread = listen.clone();
        let store_for_thread = Arc::clone(&store);
        let activity_for_thread = Arc::clone(&activity);

        std::thread::spawn(move || {
            let _ = run_server(&listen_for_thread, store_for_thread, activity_for_thread, None, None);
        });
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        let body = ureq::get(&format!("{base_url}/roc?lastId=0")).call().unwrap().into_string().unwrap();
        assert!(body.ends_with("10:01:10"), "expected time-of-day derived from time_s=36070, got: {body}");
    }

    /// Real handle_status_json against a real spawned server: confirms the
    /// running binary's build version is actually discoverable over HTTP,
    /// not just present as a Rust constant — this is what an operator (or
    /// deploy script) checks to confirm a redeploy took.
    #[test]
    fn test_status_json_reports_own_version() {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let activity = activity::new_shared();
        let listen = free_listen_addr();
        let listen_for_thread = listen.clone();
        let store_for_thread = Arc::clone(&store);
        let activity_for_thread = Arc::clone(&activity);

        std::thread::spawn(move || {
            let _ = run_server(&listen_for_thread, store_for_thread, activity_for_thread, None, None);
        });
        let base_url = format!("http://{listen}");
        wait_for_server(&base_url);

        let body = ureq::get(&format!("{base_url}/status.json")).call().unwrap().into_string().unwrap();
        assert!(
            body.contains(&format!("\"version\": \"{}\"", version::VERSION)),
            "status.json was: {body}"
        );
    }
}
