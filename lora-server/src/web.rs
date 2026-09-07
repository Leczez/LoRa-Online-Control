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

use crate::daemon_state::SharedState;
use crate::health::RadioReady;

#[derive(Serialize)]
struct NodeView {
    addr: u16,
    last_heartbeat_secs_ago: Option<u64>,
    battery_pct: Option<u8>,
    battery_mv: Option<u16>,
    last_punch_secs_ago: Option<u64>,
    last_rssi: Option<i16>,
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
fn build_status(state: &SharedState, radio_ready: &RadioReady, roc_health_url: &Option<String>) -> StatusView {
    let guard = state.lock().unwrap();
    let mut nodes: Vec<NodeView> = guard
        .nodes
        .iter()
        .map(|(&addr, s)| NodeView {
            addr,
            last_heartbeat_secs_ago: s.last_heartbeat.map(secs_ago),
            battery_pct: s.battery_pct,
            battery_mv: s.battery_mv,
            last_punch_secs_ago: s.last_punch.map(secs_ago),
            last_rssi: s.last_rssi,
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

    StatusView { radio_ready: radio_ready.load(Ordering::SeqCst), roc_reachable, nodes, log }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn render_html(v: &StatusView) -> String {
    let radio_cell = if v.radio_ready {
        "<td style=\"color:green\">ready</td>"
    } else {
        "<td style=\"color:red\">lora module not found</td>"
    };
    let roc_cell = match v.roc_reachable {
        Some(true) => "<td style=\"color:green\">reachable</td>".to_string(),
        Some(false) => "<td style=\"color:red\">unreachable</td>".to_string(),
        None => "<td>not checked (--roc-health-url unset)</td>".to_string(),
    };

    let mut node_rows = String::new();
    for n in &v.nodes {
        let battery = match (n.battery_pct, n.battery_mv) {
            (Some(pct), Some(mv)) => format!("{pct}% ({mv}mV)"),
            _ => "-".to_string(),
        };
        node_rows.push_str(&format!(
            "<tr><td>{:#06x}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            n.addr,
            n.last_heartbeat_secs_ago.map(|s| format!("{s}s ago")).unwrap_or_else(|| "-".to_string()),
            battery,
            n.last_punch_secs_ago.map(|s| format!("{s}s ago")).unwrap_or_else(|| "-".to_string()),
            n.last_rssi.map(|r| format!("{r}dBm")).unwrap_or_else(|| "-".to_string()),
        ));
    }
    if node_rows.is_empty() {
        node_rows = "<tr><td colspan=\"5\">no nodes heard from yet</td></tr>\n".to_string();
    }

    let mut log_lines = String::new();
    for entry in &v.log {
        log_lines.push_str(&format!("[-{}s] {}\n", entry.secs_ago, html_escape(&entry.line)));
    }
    if log_lines.is_empty() {
        log_lines = "(empty)".to_string();
    }

    format!(
        r#"<html><head><title>lora-server status</title></head><body>
<h1>lora-server status</h1>
<table border="1" cellpadding="4">
<tr><th>Radio</th>{radio_cell}</tr>
<tr><th>roc-server</th>{roc_cell}</tr>
</table>

<h2>Nodes</h2>
<table border="1" cellpadding="4">
<tr><th>Addr</th><th>Last heartbeat</th><th>Battery</th><th>Last punch</th><th>RSSI</th></tr>
{node_rows}
</table>

<h2>Send test punch</h2>
<p>Recorded with source="test" — flows through the real send/retry/ack
pipeline and on to roc-server/MEOS like a genuine punch, but stays tagged
as synthetic. See docs/protocols/lora_online_control_protocol.md.</p>
<form method="POST" action="/testpunch">
  <label>Card ID <input type="number" name="card_id" required></label>
  <label>Station <input type="number" name="station" required></label>
  <label>Time (s since midnight) <input type="number" name="time_s" required></label>
  <button type="submit">Send</button>
</form>

<h2>Packet log (newest first)</h2>
<pre>{log_lines}</pre>
</body></html>"#
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
    listen: String, state: SharedState, radio_ready: RadioReady, roc_health_url: Option<String>, cmd_tx: Sender<String>,
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
                        let v = build_status(&state, &radio_ready, &roc_health_url);
                        let body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
                        let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                        Response::from_string(body).with_header(header)
                    }
                    (Method::Get, "/") => {
                        let v = build_status(&state, &radio_ready, &roc_health_url);
                        let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
                        Response::from_string(render_html(&v)).with_header(header)
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
        state.lock().unwrap().record_heartbeat(10, Some((77, 3850)));
        state.lock().unwrap().push_log("RX 10 -80 HB 77 3850".to_string());

        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();

        spawn_server(listen.clone(), Arc::clone(&state), radio_ready, None, cmd_tx);

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
        assert!(json.contains("\"radio_ready\": true"), "status.json was: {json}");

        let html = ureq::get(&format!("http://{listen}/")).call().unwrap().into_string().unwrap();
        assert!(html.contains("0x000a") || html.contains("0xa"), "HTML was: {html}");
        assert!(html.contains("RX 10 -80 HB 77 3850"), "HTML was: {html}");

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
        spawn_server(listen.clone(), Arc::clone(&state), radio_ready, None, cmd_tx);

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
    }

    #[test]
    fn test_clearpunch_and_clearpunches_endpoints_drive_command_channel() {
        let state = crate::daemon_state::new_shared();
        let radio_ready = Arc::new(AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
        let listen = free_listen_addr();
        spawn_server(listen.clone(), state, radio_ready, None, cmd_tx);

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
        spawn_server(listen.clone(), state, radio_ready, None, cmd_tx);

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
