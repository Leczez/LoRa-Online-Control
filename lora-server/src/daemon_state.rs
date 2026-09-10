// lora-server/src/daemon_state.rs
//
// In-process shared state for the daemon loop: a bounded packet log and a
// per-node health table, both readable from lora-server's own web server
// (web.rs) via GET /status.json — the browser dashboard's data source and
// lora-tui's only transport (see HttpRadio in backend.rs, which polls
// /status.json and tracks each entry's seq to reconstruct a live-only
// stream client-side). log_event() in backend.rs is the sole writer.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Monotonically increasing, never reused even once the entry itself is
    /// evicted from `log` — lets a polling HTTP client (lora-tui's HttpRadio,
    /// see backend.rs) tell "new since last poll" apart from "same entry,
    /// just formatted with a fresher secs_ago", which comparing the `line`
    /// text alone can't do.
    pub seq: u64,
    pub at: SystemTime,
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub struct NodeStatus {
    pub last_heartbeat: Option<SystemTime>,
    pub battery_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    /// Whether this node's most recent heartbeat that actually reported SI
    /// status said an SI master was connected. `None` means no heartbeat
    /// from this node has ever reported SI status at all (a relay with no
    /// SI-reader concept, or older firmware) — distinct from "reported not
    /// connected", which is `Some(false)`.
    pub si_present: Option<bool>,
    pub last_punch: Option<SystemTime>,
    pub last_rssi: Option<i16>,
    /// `<semver>+<git-sha>[.dirty]` this node last reported — either its
    /// unprompted boot announcement, or a reply to a version query (see
    /// protocol.rs's `Frame::VersionReport`). `None` until it's reported at
    /// least once this daemon run — not persisted, same as the rest of this
    /// struct.
    pub version: Option<String>,
    /// Total punch *packets* (CardReadouts) received from this node — by
    /// `origin`, not radio-layer sender, so a relayed punch still counts
    /// against the control point that actually read the card, not the
    /// relay that last touched it. One packet can bundle several station
    /// taps (see sportident::CardReadout::punches); this counts packets,
    /// same granularity as last_punch/last_rssi above, not individual
    /// station punches. Since this daemon process started — not persisted
    /// across restarts, same as the rest of this struct.
    pub punch_count: u64,
}

#[derive(Default)]
pub struct DaemonState {
    pub log: VecDeque<LogEntry>,
    pub nodes: HashMap<u16, NodeStatus>,
    next_seq: u64,
}

impl DaemonState {
    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.next_seq += 1;
        self.log.push_back(LogEntry { seq: self.next_seq, at: SystemTime::now(), line });
    }

    pub fn record_heartbeat(&mut self, node: u16, battery: Option<(u8, u16)>, si_present: Option<bool>) {
        let entry = self.nodes.entry(node).or_default();
        entry.last_heartbeat = Some(SystemTime::now());
        if let Some((pct, mv)) = battery {
            entry.battery_pct = Some(pct);
            entry.battery_mv = Some(mv);
        }
        if let Some(present) = si_present {
            entry.si_present = Some(present);
        }
    }

    pub fn record_version(&mut self, node: u16, version: String) {
        self.nodes.entry(node).or_default().version = Some(version);
    }

    pub fn record_punch(&mut self, node: u16, rssi: Option<i16>) {
        let entry = self.nodes.entry(node).or_default();
        entry.last_punch = Some(SystemTime::now());
        entry.last_rssi = rssi;
        entry.punch_count += 1;
    }
}

pub type SharedState = Arc<Mutex<DaemonState>>;

pub fn new_shared() -> SharedState {
    Arc::new(Mutex::new(DaemonState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_log_evicts_oldest_past_capacity() {
        let mut state = DaemonState::default();
        for i in 0..LOG_CAPACITY + 5 {
            state.push_log(format!("line {i}"));
        }
        assert_eq!(state.log.len(), LOG_CAPACITY);
        assert_eq!(state.log.front().unwrap().line, "line 5");
        assert_eq!(state.log.back().unwrap().line, format!("line {}", LOG_CAPACITY + 4));
    }

    #[test]
    fn test_seq_keeps_increasing_past_eviction() {
        let mut state = DaemonState::default();
        for _ in 0..LOG_CAPACITY + 5 {
            state.push_log("line".to_string());
        }
        // Oldest surviving entry is the 6th pushed (1-indexed seq == 6), not
        // reset to 1 just because earlier entries were evicted.
        assert_eq!(state.log.front().unwrap().seq, 6);
        assert_eq!(state.log.back().unwrap().seq, (LOG_CAPACITY + 5) as u64);
    }

    #[test]
    fn test_record_heartbeat_preserves_battery_when_not_resent() {
        let mut state = DaemonState::default();
        state.record_heartbeat(5, Some((80, 3900)), None);
        state.record_heartbeat(5, None, None); // bare "HB", no battery data this time
        let node = &state.nodes[&5];
        assert_eq!(node.battery_pct, Some(80));
        assert_eq!(node.battery_mv, Some(3900));
    }

    #[test]
    fn test_record_heartbeat_preserves_si_present_when_not_resent() {
        let mut state = DaemonState::default();
        state.record_heartbeat(5, None, Some(true));
        state.record_heartbeat(5, None, None); // a later heartbeat that didn't report SI status
        assert_eq!(state.nodes[&5].si_present, Some(true));
    }

    #[test]
    fn test_record_heartbeat_updates_si_present_when_it_changes() {
        let mut state = DaemonState::default();
        state.record_heartbeat(5, None, Some(true));
        state.record_heartbeat(5, None, Some(false));
        assert_eq!(state.nodes[&5].si_present, Some(false));
    }

    #[test]
    fn test_record_version_tracks_separately_from_heartbeat() {
        let mut state = DaemonState::default();
        state.record_heartbeat(5, Some((80, 3900)), None);
        state.record_version(5, "0.1.0+a1b2c3d4".to_string());
        let node = &state.nodes[&5];
        assert_eq!(node.version.as_deref(), Some("0.1.0+a1b2c3d4"));
        assert_eq!(node.battery_pct, Some(80));
    }

    #[test]
    fn test_record_version_overwrites_previous_value() {
        let mut state = DaemonState::default();
        state.record_version(5, "0.1.0+a1b2c3d4".to_string());
        state.record_version(5, "0.1.0+deadbeef.dirty".to_string());
        assert_eq!(state.nodes[&5].version.as_deref(), Some("0.1.0+deadbeef.dirty"));
    }

    #[test]
    fn test_record_punch_tracks_separately_from_heartbeat() {
        let mut state = DaemonState::default();
        state.record_punch(7, Some(-80));
        let node = &state.nodes[&7];
        assert!(node.last_punch.is_some());
        assert!(node.last_heartbeat.is_none());
        assert_eq!(node.last_rssi, Some(-80));
        assert_eq!(node.punch_count, 1);
    }

    #[test]
    fn test_record_punch_increments_count_per_node() {
        let mut state = DaemonState::default();
        state.record_punch(7, Some(-80));
        state.record_punch(7, Some(-75));
        state.record_punch(9, Some(-60));

        assert_eq!(state.nodes[&7].punch_count, 2);
        assert_eq!(state.nodes[&9].punch_count, 1);
        // Most recent rssi wins, count doesn't reset it.
        assert_eq!(state.nodes[&7].last_rssi, Some(-75));
    }
}
