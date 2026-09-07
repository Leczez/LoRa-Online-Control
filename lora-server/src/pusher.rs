// lora-server/src/pusher.rs
//
// Background thread that drains the persistent punch buffer to the remote
// roc/mip output server whenever it's reachable. Runs independently of LoRa
// receiving and local SI reading — a network outage or a down output server
// only delays delivery (punches stay in the buffer, unsent), it never blocks
// or affects the daemon's own reception/logging.

use crate::punch_buffer::PunchBuffer;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize)]
struct PunchPush<'a> {
    card_id: u32,
    station: u8,
    time_s: u32,
    source: &'a str,
}

/// Spawns the pusher thread. `push_url` is the output server's ingestion
/// endpoint, e.g. `http://100.x.y.z:8080/punches`.
pub fn spawn(buffer: Arc<PunchBuffer>, push_url: String, poll_interval: Duration) {
    std::thread::Builder::new()
        .name("punch-pusher".into())
        .spawn(move || loop {
            match buffer.unsent() {
                Ok(unsent) => {
                    for punch in unsent {
                        let body = PunchPush {
                            card_id: punch.card_id,
                            station: punch.station,
                            time_s: punch.time_s,
                            source: &punch.source,
                        };
                        match ureq::post(&push_url)
                            .timeout(Duration::from_secs(5))
                            .send_json(&body)
                        {
                            Ok(_) => {
                                if let Err(e) = buffer.mark_sent(punch.id) {
                                    log::error!("failed to mark punch {} sent: {}", punch.id, e);
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "push to output server failed ({}), will retry: {}",
                                    push_url, e
                                );
                                // Stop this pass; remaining unsent punches stay
                                // queued and are retried on the next tick.
                                break;
                            }
                        }
                    }
                }
                Err(e) => log::error!("failed to read punch buffer: {}", e),
            }

            std::thread::sleep(poll_interval);
        })
        .expect("failed to spawn punch-pusher thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// Exercises the real `spawn()` code path end to end — not a hand-built
    /// proxy for it — against a minimal in-process HTTP listener (raw
    /// TcpListener, not a full HTTP server dependency: just enough parsing
    /// to capture the request body pusher.rs actually sends). Confirms both
    /// halves: the exact JSON it POSTs, and that a 200 response correctly
    /// drives `mark_sent` so the punch drops out of `unsent()`.
    #[test]
    fn test_pusher_sends_real_request_and_marks_sent_on_success() {
        let buffer = Arc::new(PunchBuffer::open(":memory:").unwrap());
        let id = buffer.record(123456, 31, 36070, "local").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (body_tx, body_rx) = mpsc::channel::<String>();

        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let _ = body_tx.send(String::from_utf8(body).unwrap());

            let mut stream = stream;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        });

        spawn(Arc::clone(&buffer), format!("http://{}/punches", addr), Duration::from_millis(50));

        let body = body_rx.recv_timeout(Duration::from_secs(2)).expect("pusher never sent a request");
        assert!(body.contains("\"card_id\":123456"), "body was: {body}");
        assert!(body.contains("\"station\":31"), "body was: {body}");
        assert!(body.contains("\"time_s\":36070"), "body was: {body}");
        assert!(body.contains("\"source\":\"local\""), "body was: {body}");

        // Give the pusher thread a moment to process the response.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let unsent = buffer.unsent().unwrap();
            if !unsent.iter().any(|p| p.id == id) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "punch was never marked sent");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
