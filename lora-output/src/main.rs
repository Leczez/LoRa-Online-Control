mod mip;
mod roc;
mod store;

use anyhow::Result;
use clap::Parser;
use serde::Deserialize;
use std::sync::Arc;
use tiny_http::{Header, Method, Response, Server};

use store::Store;

#[derive(Parser, Debug)]
#[command(name = "lora-output", about = "MIP/ROC output server for buffered LoRa punches")]
struct Args {
    /// Address:port to listen on.
    #[arg(long, env = "LORA_OUTPUT_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,

    /// Path to this server's own SQLite punch log.
    #[arg(long, env = "LORA_OUTPUT_DB", default_value = "/var/lib/lora-output/punches.db")]
    db: String,
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

    log::info!("lora-output listening on {} (db {})", args.listen, args.db);
    let server = Server::http(&args.listen).map_err(|e| anyhow::anyhow!("{e}"))?;

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();

        let response = match (&method, path.as_str()) {
            (Method::Post, "/punches") => handle_punches(&mut request, &store),
            (Method::Get, "/mip") => handle_mip(&request, &url, &store),
            (Method::Get, "/roc") => handle_roc(&url, &store),
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

fn handle_punches(request: &mut tiny_http::Request, store: &Store) -> Response<std::io::Cursor<Vec<u8>>> {
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

fn handle_mip(request: &tiny_http::Request, url: &str, store: &Store) -> Response<std::io::Cursor<Vec<u8>>> {
    let last_id = last_id_from_request(request, url);
    match store.since(last_id) {
        Ok(punches) => {
            let new_last_id = punches.last().map(|p| p.id).unwrap_or(last_id);
            xml_response(mip::render_mip_xml(new_last_id, &punches))
        }
        Err(e) => {
            log::error!("failed to query store for /mip: {e}");
            text_response(500, "internal error")
        }
    }
}

fn handle_roc(url: &str, store: &Store) -> Response<std::io::Cursor<Vec<u8>>> {
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
            text_response(200, &roc::render_roc_text(&punches, &timestamps))
        }
        Err(e) => {
            log::error!("failed to query store for /roc: {e}");
            text_response(500, "internal error")
        }
    }
}
