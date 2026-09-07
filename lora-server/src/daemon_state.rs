// lora-server/src/daemon_state.rs
//
// In-process shared state for the daemon loop: a bounded packet log and a
// per-node health table, both readable from lora-server's own web server
// (web.rs) for the status dashboard. Deliberately separate from the
// existing Unix-socket broadcast() mechanism lora-tui uses — that's a
// live-only stream (a client attaching after the fact sees nothing until
// new traffic arrives), whereas a web dashboard needs to show *current*
// state to a browser that just loaded the page. log_and_broadcast() in
// backend.rs feeds both from the same call sites.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: SystemTime,
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub struct NodeStatus {
    pub last_heartbeat: Option<SystemTime>,
    pub battery_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    pub last_punch: Option<SystemTime>,
    pub last_rssi: Option<i16>,
}

#[derive(Default)]
pub struct DaemonState {
    pub log: VecDeque<LogEntry>,
    pub nodes: HashMap<u16, NodeStatus>,
}

impl DaemonState {
    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry { at: SystemTime::now(), line });
    }

    pub fn record_heartbeat(&mut self, node: u16, battery: Option<(u8, u16)>) {
        let entry = self.nodes.entry(node).or_default();
        entry.last_heartbeat = Some(SystemTime::now());
        if let Some((pct, mv)) = battery {
            entry.battery_pct = Some(pct);
            entry.battery_mv = Some(mv);
        }
    }

    pub fn record_punch(&mut self, node: u16, rssi: Option<i16>) {
        let entry = self.nodes.entry(node).or_default();
        entry.last_punch = Some(SystemTime::now());
        entry.last_rssi = rssi;
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
    fn test_record_heartbeat_preserves_battery_when_not_resent() {
        let mut state = DaemonState::default();
        state.record_heartbeat(5, Some((80, 3900)));
        state.record_heartbeat(5, None); // bare "HB", no battery data this time
        let node = &state.nodes[&5];
        assert_eq!(node.battery_pct, Some(80));
        assert_eq!(node.battery_mv, Some(3900));
    }

    #[test]
    fn test_record_punch_tracks_separately_from_heartbeat() {
        let mut state = DaemonState::default();
        state.record_punch(7, Some(-80));
        let node = &state.nodes[&7];
        assert!(node.last_punch.is_some());
        assert!(node.last_heartbeat.is_none());
        assert_eq!(node.last_rssi, Some(-80));
    }
}
