// lora-cli/src/pusher.rs
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
