// lora-server/src/web.rs
//
// This IS lora-tui's transport, not just a browser dashboard: GET
// /status.json (radio/roc-server reachability, node health table, recent
// packet log with monotonic seq numbers for polling clients) plus GET / for
// a browser view of the same data. POST /send, /setdest, /cmd, /testpunch
// all forward to the daemon's own command channel (cmd_tx, read by
// run_daemon_loop in backend.rs) — lora-tui's HttpRadio (backend.rs) and a
// human using the HTML form are just two more callers of the same commands.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tiny_http::{Header, Method, Response, Server};

use crate::backend::SharedCompetitionId;
use crate::daemon_state::SharedState;
use crate::health::RadioReady;

#[derive(Serialize)]
struct NodeView {
    addr: u16,
    last_heartbeat_secs_ago: Option<u64>,
    battery_pct: Option<u8>,
    battery_mv: Option<u16>,
    /// See daemon_state::NodeStatus::si_present — null means never reported
    /// (not every node has an SI reader), distinct from `false`.
    si_present: Option<bool>,
    last_punch_secs_ago: Option<u64>,
    last_rssi: Option<i16>,
    punch_count: u64,
    /// See daemon_state::NodeStatus::version — null until this node has
    /// reported at least once (boot announcement or a /queryversion reply).
    version: Option<String>,
}

#[derive(Serialize)]
struct LogLineView {
    /// See daemon_state::LogEntry::seq — lora-tui's HttpRadio polls this
    /// endpoint and uses seq to tell which lines are new since its last
    /// poll, since `line` text alone repeats and secs_ago changes every call.
    seq: u64,
    secs_ago: u64,
    line: String,
}

#[derive(Serialize)]
struct StatusView {
    /// This daemon's own configured LoRa address — lets a polling client
    /// (lora-tui's HttpRadio) discover it authoritatively instead of
    /// relying on its own possibly-stale/mismatched CLI default (see
    /// backend.rs::attach's doc comment).
    own_addr: u16,
    /// `<semver>+<git-sha>[.dirty]` — see version.rs. Lets an operator (or a
    /// deploy script) confirm a redeploy actually took by comparing this
    /// against the commit they just pushed, without SSHing in.
    version: &'static str,
    /// The competition ID currently being sent with every pushed punch (see
    /// pusher.rs) — runtime-changeable via POST /setcompetitionid, so this
    /// is the live value, not necessarily what --competition-id was set to
    /// at boot.
    competition_id: String,
    radio_ready: bool,
    roc_reachable: Option<bool>,
    nodes: Vec<NodeView>,
    log: Vec<LogLineView>,
}

fn secs_ago(t: SystemTime) -> u64 {
    SystemTime::now().duration_since(t).unwrap_or_default().as_secs()
}

/// Checks roc-server live, on each page load, rather than caching a
/// background checker thread's state here too — avoids two different
/// notions of "reachable" needing to agree, at the cost of a sub-second
/// HTTP round trip per dashboard load.
fn build_status(
    own_addr: u16, state: &SharedState, radio_ready: &RadioReady, roc_health_url: &Option<String>,
    competition_id: &SharedCompetitionId,
) -> StatusView {
    let guard = state.lock().unwrap();
    let mut nodes: Vec<NodeView> = guard
        .nodes
        .iter()
        .map(|(&addr, s)| NodeView {
            addr,
            last_heartbeat_secs_ago: s.last_heartbeat.map(secs_ago),
            battery_pct: s.battery_pct,
            battery_mv: s.battery_mv,
            si_present: s.si_present,
            last_punch_secs_ago: s.last_punch.map(secs_ago),
            last_rssi: s.last_rssi,
            version: s.version.clone(),
            punch_count: s.punch_count,
        })
        .collect();
    nodes.sort_by_key(|n| n.addr);

    let log: Vec<LogLineView> = guard
        .log
        .iter()
        .rev()
        .map(|e| LogLineView { seq: e.seq, secs_ago: secs_ago(e.at), line: e.line.clone() })
        .collect();
    drop(guard);

    let roc_reachable = roc_health_url
        .as_ref()
        .map(|url| ureq::get(url).timeout(Duration::from_secs(2)).call().is_ok());

    StatusView {
        own_addr,
        version: crate::version::VERSION,
        competition_id: competition_id.lock().unwrap().clone(),
        radio_ready: radio_ready.load(Ordering::SeqCst),
        roc_reachable,
        nodes,
        log,
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Shared look for this dashboard and roc-server's (see roc-server/src/
/// main.rs's identical copy — no shared crate between the two binaries to
/// hang a single definition off of). Kept as its own constant rather than
/// inline in the format! template below because format! parses `{`/`}` in
/// its *template* string as placeholders — a raw CSS block full of literal
/// braces would need every one doubled if it were part of that string
/// directly. A substituted argument's own contents (this constant) aren't
/// re-parsed, so no escaping is needed here.
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
form label { display: block; font-size: .85rem; margin-bottom: .65rem; }
form input {
  display: block; margin-top: .3rem; padding: .4rem .55rem; border: 1px solid var(--border);
  border-radius: 6px; font-size: .9rem; width: 100%; max-width: 220px;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
button {
  background: var(--accent); color: #fff; border: none; padding: .5rem 1.1rem;
  border-radius: 6px; font-size: .88rem; font-weight: 600; cursor: pointer; margin-top: .3rem;
}
button:hover { opacity: .9; }
pre.log {
  background: #0b1020; color: #d7dde8; padding: 1rem; border-radius: 10px;
  font-size: .76rem; line-height: 1.6; overflow: auto; max-height: 440px; margin: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
</style>"#;

fn pill(good: bool, text: &str) -> String {
    format!("<span class=\"pill {}\">{}</span>", if good { "pill-good" } else { "pill-bad" }, text)
}

fn render_html(v: &StatusView) -> String {
    let own_addr = v.own_addr;
    let version = html_escape(v.version);
    let competition_id = html_escape(&v.competition_id);
    let radio_cell = if v.radio_ready {
        pill(true, "ready")
    } else {
        pill(false, "lora module not found")
    };
    let roc_cell = match v.roc_reachable {
        Some(true) => pill(true, "reachable"),
        Some(false) => pill(false, "unreachable"),
        None => "<span class=\"pill pill-warn\">not checked (--roc-health-url unset)</span>".to_string(),
    };

    let mut node_rows = String::new();
    for n in &v.nodes {
        let battery = match (n.battery_pct, n.battery_mv) {
            (Some(pct), Some(mv)) => format!("{pct}% ({mv}mV)"),
            _ => "-".to_string(),
        };
        let si_cell = match n.si_present {
            Some(true) => pill(true, "connected"),
            Some(false) => pill(false, "not connected"),
            None => "-".to_string(),
        };
        node_rows.push_str(&format!(
            "<tr><td class=\"mono\">{:#06x}</td><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td>{}</td><td class=\"mono\">{}</td><td class=\"mono\">{}</td><td class=\"mono\">{}</td></tr>\n",
            n.addr,
            n.last_heartbeat_secs_ago.map(|s| format!("{s}s ago")).unwrap_or_else(|| "-".to_string()),
            battery,
            si_cell,
            n.last_punch_secs_ago.map(|s| format!("{s}s ago")).unwrap_or_else(|| "-".to_string()),
            n.last_rssi.map(|r| format!("{r}dBm")).unwrap_or_else(|| "-".to_string()),
            n.punch_count,
            n.version.as_deref().map(html_escape).unwrap_or_else(|| "-".to_string()),
        ));
    }
    if node_rows.is_empty() {
        node_rows = "<tr><td colspan=\"8\">no nodes heard from yet</td></tr>\n".to_string();
    }

    let mut log_lines = String::new();
    for entry in &v.log {
        log_lines.push_str(&format!("[-{}s] {}\n", entry.secs_ago, html_escape(&entry.line)));
    }
    if log_lines.is_empty() {
        log_lines = "(empty)".to_string();
    }

    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="10">
<title>lora-server status</title>
{DASHBOARD_STYLE}
</head>
<body>
<div class="wrap">
<h1>lora-server</h1>
<p class="subtitle">LoRa daemon status &amp; node health — this node: <span class="mono">{own_addr:#06x}</span> · <span class="mono">{version}</span></p>

<h2>Status</h2>
<div class="card">
<table>
<tr><th>Radio</th><td>{radio_cell}</td></tr>
<tr><th>roc-server</th><td>{roc_cell}</td></tr>
<tr><th>Competition</th><td class="mono">{competition_id}</td></tr>
</table>
</div>

<h2>Competition ID</h2>
<div class="card">
<p class="hint">Sent with every punch pushed to roc-server, which rejects a mismatch
against its own configured value — must equal whatever MEOS's Online Input dialog
is configured with. Change this at the start of a new event; the previous
event's punches (which roc-server keeps around) will never leak into a
different competition ID's results. Takes effect immediately, no restart needed.</p>
<form method="POST" action="/setcompetitionid">
  <label>Competition ID <input type="text" name="competition_id" value="{competition_id}" required></label>
  <button type="submit">Save</button>
</form>
</div>

<h2>Nodes</h2>
<div class="card">
<table>
<tr><th>Addr</th><th>Last heartbeat</th><th>Battery</th><th>SI master</th><th>Last punch</th><th>RSSI</th><th>Punches</th><th>Version</th></tr>
{node_rows}
</table>
</div>

<h2>Query node version</h2>
<div class="card">
<p class="hint">Asks a node to report its firmware version right now, rather than
waiting for its next boot (nodes also report once, unprompted, on every boot —
see the Version column above). Reply lands asynchronously; refresh to see it.</p>
<form method="POST" action="/queryversion">
  <label>Target address <input type="number" name="target" required></label>
  <button type="submit">Query version</button>
</form>
</div>

<h2>Send test punch</h2>
<div class="card">
<p class="hint">Recorded with source="test" — flows through the real send/retry/ack
pipeline and on to roc-server/MEOS like a genuine punch, but stays tagged
as synthetic. See docs/protocols/lora_online_control_protocol.md.</p>
<form method="POST" action="/testpunch">
  <label>Card ID <input type="number" name="card_id" required></label>
  <label>Station <input type="number" name="station" required></label>
  <label>Time (s since midnight) <input type="number" name="time_s" required></label>
  <button type="submit">Send test punch</button>
</form>
</div>

<h2>Packet log <span class="note">(newest first, auto-refreshes)</span></h2>
<pre class="log">{log_lines}</pre>
</div>
</body>
</html>"#
    )
}

fn parse_form(body: &str) -> HashMap<&str, &str> {
    body.split('&').filter_map(|pair| pair.split_once('=')).collect()
}

/// The command channel's only failure mode is the daemon loop's receiver
/// having been dropped, i.e. run_daemon_loop panicked or exited — the
/// command was NOT applied. Callers must not report success in that case,
/// even though the send() itself "succeeded" from cmd_tx's point of view.
fn command_channel_down() -> Response<std::io::Cursor<Vec<u8>>> {
    log::error!("dropped a command: daemon command channel receiver is gone");
    Response::from_string("daemon command loop is not running").with_status_code(503)
}

/// Spawns the browser-facing status dashboard. `roc_health_url` mirrors
/// whatever `--roc-health-url` was configured with (may be the same value
/// health::spawn_checker already uses) — `None` means "don't check,
/// roc-server reachability just won't be shown."
pub fn spawn_server(
    listen: String, own_addr: u16, state: SharedState, radio_ready: RadioReady, roc_health_url: Option<String>,
    cmd_tx: Sender<String>, competition_id: SharedCompetitionId,
) {
    std::thread::Builder::new()
        .name("web-server".into())
        .spawn(move || {
            let server = match Server::http(&listen) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("web dashboard failed to bind {}: {}", listen, e);
                    return;
                }
            };
            log::info!("web dashboard listening on {}", listen);

            for mut request in server.incoming_requests() {
                let method = request.method().clone();
                let url = request.url().to_string();
                let path = url.split('?').next().unwrap_or("").to_string();

                let response = match (&method, path.as_str()) {
                    (Method::Get, "/status.json") => {
                        let v = build_status(own_addr, &state, &radio_ready, &roc_health_url, &competition_id);
                        let body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
                        let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                        Response::from_string(body).with_header(header)
                    }
                    (Method::Get, "/") => {
                        let v = build_status(own_addr, &state, &radio_ready, &roc_health_url, &competition_id);
                        let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                        Response::from_string(render_html(&v)).with_header(header)
                    }
                    // Runtime-changeable without a restart, unlike most of
                    // this daemon's other settings — see
                    // backend::SharedCompetitionId's doc comment for why
                    // this writes straight to the shared value instead of
                    // going through cmd_tx like /setdest, /cmd, etc.
                    (Method::Post, "/setcompetitionid") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match form.get("competition_id").filter(|s| !s.is_empty()) {
                            Some(id) => {
                                *competition_id.lock().unwrap() = id.to_string();
                                log::info!("competition id changed to {:?}", id);
                                let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                                Response::from_string("<html><body>Saved. <a href=\"/\">Back</a></body></html>").with_header(header)
                            }
                            None => Response::from_string("missing/empty competition_id").with_status_code(400),
                        }
                    }
                    (Method::Post, "/testpunch") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match (form.get("card_id"), form.get("station"), form.get("time_s")) {
                            (Some(c), Some(s), Some(t)) => {
                                match cmd_tx.send(format!("TESTPUNCH {} {} {}", c, s, t)) {
                                    Ok(()) => {
                                        let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                                        Response::from_string("<html><body>Sent. <a href=\"/\">Back</a></body></html>").with_header(header)
                                    }
                                    Err(_) => command_channel_down(),
                                }
                            }
                            _ => Response::from_string("missing card_id/station/time_s").with_status_code(400),
                        }
                    }
                    // Raw text body, not form-encoded — a radio payload can
                    // contain '&'/'=' that the naive parse_form() splitter
                    // (used everywhere else here) would mangle.
                    (Method::Post, "/send") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let payload = body.trim_end_matches(['\r', '\n']);
                        if payload.is_empty() {
                            Response::from_string("empty payload").with_status_code(400)
                        } else {
                            match cmd_tx.send(format!("SEND {}", payload)) {
                                Ok(()) => Response::from_string("ok"),
                                Err(_) => command_channel_down(),
                            }
                        }
                    }
                    // Hex-encoded body — byte-safe counterpart to /send,
                    // used internally by HttpRadio::send (see backend.rs)
                    // rather than by a human operator: a payload here can
                    // be one of the binary hot-path frames (PUNCH/PACK/HB),
                    // which /send's plain-text contract can't carry without
                    // corrupting it. Validated as hex here so a malformed
                    // body 400s immediately instead of silently failing
                    // deeper in the daemon loop.
                    (Method::Post, "/sendbin") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let hex = body.trim();
                        if hex.is_empty() || crate::backend::from_hex(hex).is_none() {
                            Response::from_string("empty or invalid hex payload").with_status_code(400)
                        } else {
                            match cmd_tx.send(format!("SENDBIN {}", hex)) {
                                Ok(()) => Response::from_string("ok"),
                                Err(_) => command_channel_down(),
                            }
                        }
                    }
                    (Method::Post, "/setdest") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match form.get("dest").and_then(|s| s.parse::<u16>().ok()) {
                            Some(dest) => match cmd_tx.send(format!("SET_DEST {}", dest)) {
                                Ok(()) => Response::from_string("ok"),
                                Err(_) => command_channel_down(),
                            },
                            None => Response::from_string("missing/invalid dest").with_status_code(400),
                        }
                    }
                    (Method::Post, "/cmd") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match (
                            form.get("target").and_then(|s| s.parse::<u16>().ok()),
                            form.get("heartbeat_interval_secs").and_then(|s| s.parse::<u32>().ok()),
                        ) {
                            (Some(target), Some(secs)) => match cmd_tx.send(format!("CMD {} {}", target, secs)) {
                                Ok(()) => Response::from_string("ok"),
                                Err(_) => command_channel_down(),
                            },
                            _ => Response::from_string("missing/invalid target/heartbeat_interval_secs").with_status_code(400),
                        }
                    }
                    // Generic counterpart to /cmd for any Setting (see
                    // protocol.rs) — form field `setting` is the same
                    // key=value text Setting::encode() produces (e.g.
                    // "sf=11", "settings_mode=1"), validated here so a typo
                    // 400s immediately instead of silently vanishing into
                    // the command channel with no feedback.
                    (Method::Post, "/setcfg") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match (
                            form.get("target").and_then(|s| s.parse::<u16>().ok()),
                            form.get("setting").and_then(|s| crate::protocol::Setting::parse(s)),
                        ) {
                            (Some(target), Some(setting)) => {
                                match cmd_tx.send(format!("SETCFG {} {}", target, setting.encode())) {
                                    Ok(()) => Response::from_string("ok"),
                                    Err(_) => command_channel_down(),
                                }
                            }
                            _ => Response::from_string("missing/invalid target/setting").with_status_code(400),
                        }
                    }
                    // Returns the same small "Sent. Back" HTML as /testpunch
                    // (not a bare "ok" like /setdest, /cmd, etc.) since this
                    // is driven by an actual browser form submission in the
                    // dashboard, not just an API caller.
                    (Method::Post, "/queryversion") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match form.get("target").and_then(|s| s.parse::<u16>().ok()) {
                            Some(target) => match cmd_tx.send(format!("QUERYVERSION {}", target)) {
                                Ok(()) => {
                                    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                                    Response::from_string("<html><body>Sent. <a href=\"/\">Back</a></body></html>").with_header(header)
                                }
                                Err(_) => command_channel_down(),
                            },
                            None => Response::from_string("missing/invalid target").with_status_code(400),
                        }
                    }
                    (Method::Post, "/clearpunch") => {
                        let mut body = String::new();
                        let _ = request.as_reader().read_to_string(&mut body);
                        let form = parse_form(&body);
                        match form.get("id").and_then(|s| s.parse::<i64>().ok()) {
                            Some(id) => match cmd_tx.send(format!("CLEARPUNCH {}", id)) {
                                Ok(()) => Response::from_string("ok"),
                                Err(_) => command_channel_down(),
                            },
                            None => Response::from_string("missing/invalid id").with_status_code(400),
                        }
                    }
                    (Method::Post, "/clearpunches") => match cmd_tx.send("CLEARPUNCHES".to_string()) {
                        Ok(()) => Response::from_string("ok"),
                        Err(_) => command_channel_down(),
                    },
                    _ => Response::from_string("not found").with_status_code(404),
                };

                if let Err(e) = request.respond(response) {
                    log::warn!("failed to respond to web dashboard request: {}", e);
                }
            }
        })
        .expect("failed to spawn web dashboard thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};

    fn free_listen_addr() -> String {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("127.0.0.1:{port}")
    }

    /// Real spawn_server() against real HTTP requests: confirms node data
    /// recorded via daemon_state actually surfaces through /status.json,
    /// and that /testpunch really drives the command channel run_daemon_loop
    /// reads TESTPUNCH from — not just that this all compiles.
    #[test]
    fn test_status_json_and_testpunch_against_real_server() {
        let state = crate::daemon_state::new_shared();
        state.lock().unwrap().record_heartbeat(10, Some((77, 3850)), Some(true));
        state.lock().unwrap().record_version(10, "0.1.0+deadbeef".to_string());
        state.lock().unwrap().push_log("RX 10 -80 HB 77 3850".to_string());

        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();

        spawn_server(listen.clone(), 10, Arc::clone(&state), radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let json = loop {
            match ureq::get(&format!("http://{listen}/status.json")).call() {
                Ok(resp) => break resp.into_string().unwrap(),
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("web dashboard never came up: {e}"),
            }
        };
        assert!(json.contains("\"addr\": 10"), "status.json was: {json}");
        assert!(json.contains("\"battery_pct\": 77"), "status.json was: {json}");
        assert!(json.contains("\"si_present\": true"), "status.json was: {json}");
        assert!(json.contains("\"radio_ready\": true"), "status.json was: {json}");
        assert!(
            json.contains(&format!("\"version\": \"{}\"", crate::version::VERSION)),
            "status.json was: {json}"
        );
        assert!(json.contains("\"version\": \"0.1.0+deadbeef\""), "status.json was: {json}");

        let html = ureq::get(&format!("http://{listen}/")).call().unwrap().into_string().unwrap();
        assert!(html.contains("0x000a") || html.contains("0xa"), "HTML was: {html}");
        assert!(html.contains("RX 10 -80 HB 77 3850"), "HTML was: {html}");
        assert!(html.contains("connected"), "HTML was: {html}");
        assert!(html.contains("0.1.0+deadbeef"), "HTML was: {html}");

        ureq::post(&format!("http://{listen}/testpunch"))
            .send_string("card_id=555&station=9&time_s=1234")
            .unwrap();
        let cmd = cmd_rx.recv_timeout(Duration::from_secs(2)).expect("testpunch never reached the command channel");
        assert_eq!(cmd, "TESTPUNCH 555 9 1234");
    }

    /// The three endpoints lora-tui's HttpRadio relies on for everything
    /// besides status polling and test punches: confirms each forwards the
    /// exact command-channel line run_daemon_loop's cmd_rx match (backend.rs)
    /// already parses, and that log entries carry a seq number a polling
    /// client can diff against.
    #[test]
    fn test_send_setdest_cmd_endpoints_drive_command_channel() {
        let state = crate::daemon_state::new_shared();
        state.lock().unwrap().push_log("first".to_string());
        state.lock().unwrap().push_log("second".to_string());

        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        spawn_server(listen.clone(), 10, Arc::clone(&state), radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let json = loop {
            match ureq::get(&format!("http://{listen}/status.json")).call() {
                Ok(resp) => break resp.into_string().unwrap(),
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("web dashboard never came up: {e}"),
            }
        };
        assert!(json.contains("\"seq\": 1") && json.contains("\"seq\": 2"), "status.json was: {json}");

        ureq::post(&format!("http://{listen}/send")).send_string("PUNCH 5 123456 31:100").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "SEND PUNCH 5 123456 31:100");

        ureq::post(&format!("http://{listen}/setdest")).send_string("dest=7").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "SET_DEST 7");

        ureq::post(&format!("http://{listen}/cmd")).send_string("target=5&heartbeat_interval_secs=60").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "CMD 5 60");

        ureq::post(&format!("http://{listen}/queryversion")).send_string("target=10").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "QUERYVERSION 10");
    }

    /// /setcfg is the generic counterpart to /cmd for any Setting (see
    /// protocol.rs) — a valid target+setting reaches the command channel
    /// exactly as SETCFG's own parser expects, and an unrecognized setting
    /// key 400s immediately rather than silently vanishing.
    #[test]
    fn test_setcfg_endpoint_drives_command_channel_and_rejects_bad_setting() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        spawn_server(listen.clone(), 10, state, radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        ureq::post(&format!("http://{listen}/setcfg")).send_string("target=10&setting=sf=11").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "SETCFG 10 sf=11");

        let bad = ureq::post(&format!("http://{listen}/setcfg")).send_string("target=10&setting=nonsense=1");
        assert_eq!(bad.unwrap_err().into_response().unwrap().status(), 400);
        assert!(cmd_rx.recv_timeout(Duration::from_millis(200)).is_err(), "an unrecognized setting must not reach the command channel");
    }

    /// /sendbin is the byte-safe counterpart to /send (see HttpRadio::send
    /// in backend.rs) — a hex body round-trips into a `SENDBIN <hex>`
    /// command line unchanged, and a malformed body is rejected with 400
    /// before it ever reaches the command channel.
    #[test]
    fn test_sendbin_endpoint_drives_command_channel_and_rejects_bad_hex() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        spawn_server(listen.clone(), 10, state, radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        let resp = ureq::post(&format!("http://{listen}/sendbin")).send_string("0102ff").unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "SENDBIN 0102ff");

        let bad = ureq::post(&format!("http://{listen}/sendbin")).send_string("not hex");
        assert_eq!(bad.unwrap_err().into_response().unwrap().status(), 400);
        assert!(cmd_rx.recv_timeout(Duration::from_millis(200)).is_err(), "invalid hex must not reach the command channel");
    }

    /// /setcompetitionid writes straight to the shared value (not via
    /// cmd_tx like the endpoints above) — confirms a real POST actually
    /// changes what /status.json reports immediately after, and that an
    /// empty value is rejected rather than silently accepted (an empty
    /// competition_id could never match anything roc-server checks against,
    /// so accepting it would just mean every future push/poll starts
    /// failing until someone notices).
    #[test]
    fn test_setcompetitionid_endpoint_changes_live_value() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, _cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        let competition_id = Arc::new(std::sync::Mutex::new("evt-1".to_string()));
        spawn_server(listen.clone(), 10, state, radio_ready, None, cmd_tx, Arc::clone(&competition_id));
        let base_url = format!("http://{listen}");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match ureq::get(&format!("{base_url}/status.json")).call() {
                Ok(resp) => {
                    assert!(resp.into_string().unwrap().contains("\"competition_id\": \"evt-1\""));
                    break;
                }
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("web dashboard never came up: {e}"),
            }
        }

        let resp = ureq::post(&format!("{base_url}/setcompetitionid")).send_string("competition_id=evt-2").unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(*competition_id.lock().unwrap(), "evt-2");
        let json = ureq::get(&format!("{base_url}/status.json")).call().unwrap().into_string().unwrap();
        assert!(json.contains("\"competition_id\": \"evt-2\""), "status.json was: {json}");

        let empty = ureq::post(&format!("{base_url}/setcompetitionid")).send_string("competition_id=");
        assert_eq!(empty.unwrap_err().into_response().unwrap().status(), 400);
        assert_eq!(*competition_id.lock().unwrap(), "evt-2", "empty value should not have overwritten the real one");
    }

    #[test]
    fn test_clearpunch_and_clearpunches_endpoints_drive_command_channel() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        spawn_server(listen.clone(), 10, state, radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match ureq::get(&format!("http://{listen}/status.json")).call() {
                Ok(_) => break,
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("web dashboard never came up: {e}"),
            }
        }

        ureq::post(&format!("http://{listen}/clearpunch")).send_string("id=42").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "CLEARPUNCH 42");

        ureq::post(&format!("http://{listen}/clearpunches")).send_string("").unwrap();
        assert_eq!(cmd_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "CLEARPUNCHES");
    }

    /// If the daemon loop's receiver is gone (it panicked or exited), a
    /// command send truly failed — every command endpoint must report that
    /// as a 503, not silently answer "ok" as if the command had gone
    /// through (see command_channel_down's doc comment).
    #[test]
    fn test_command_endpoints_503_when_command_channel_receiver_is_gone() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        drop(cmd_rx); // simulates the daemon loop having died
        let listen = free_listen_addr();
        spawn_server(listen.clone(), 10, state, radio_ready, None, cmd_tx, Arc::new(std::sync::Mutex::new("evt-1".to_string())));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match ureq::get(&format!("http://{listen}/status.json")).call() {
                Ok(_) => break,
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("web dashboard never came up: {e}"),
            }
        }

        for (path, body) in [
            ("/send", "PUNCH 5 123456 31:100".to_string()),
            ("/setdest", "dest=7".to_string()),
            ("/cmd", "target=5&heartbeat_interval_secs=60".to_string()),
            ("/testpunch", "card_id=1&station=2&time_s=3".to_string()),
        ] {
            match ureq::post(&format!("http://{listen}{path}")).send_string(&body) {
                Err(ureq::Error::Status(503, _)) => {}
                other => panic!("{path} did not 503 with a dead command channel: {other:?}"),
            }
        }
    }
}
