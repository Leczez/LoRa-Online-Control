// roc-server/src/activity.rs
//
// In-process activity tracking for the web dashboard: a bounded request
// log, and a "last seen per source" table built from the `source` field
// every pushed punch already carries (lora-server's own node address, or
// "local"/"test" — see lora-server/src/punch_buffer.rs). Self-contained
// deliberately: this doesn't fetch lora-server's own dashboard/state over
// HTTP, so it keeps working even if that's unreachable — it only reflects
// what roc-server has actually observed itself.

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
pub struct SourceStatus {
    pub last_seen: Option<SystemTime>,
    pub punch_count: u64,
}

#[derive(Default)]
pub struct ActivityState {
    pub log: VecDeque<LogEntry>,
    pub sources: HashMap<String, SourceStatus>,
}

impl ActivityState {
    pub fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry { at: SystemTime::now(), line });
    }

    pub fn record_source(&mut self, source: &str) {
        let entry = self.sources.entry(source.to_string()).or_default();
        entry.last_seen = Some(SystemTime::now());
        entry.punch_count += 1;
    }
}

pub type SharedActivity = Arc<Mutex<ActivityState>>;

pub fn new_shared() -> SharedActivity {
    Arc::new(Mutex::new(ActivityState::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_source_tracks_count_and_last_seen() {
        let mut state = ActivityState::default();
        state.record_source("local");
        state.record_source("local");
        state.record_source("10");

        assert_eq!(state.sources["local"].punch_count, 2);
        assert_eq!(state.sources["10"].punch_count, 1);
        assert!(state.sources["local"].last_seen.is_some());
    }

    #[test]
    fn test_push_log_evicts_oldest_past_capacity() {
        let mut state = ActivityState::default();
        for i in 0..LOG_CAPACITY + 3 {
            state.push_log(format!("entry {i}"));
        }
        assert_eq!(state.log.len(), LOG_CAPACITY);
        assert_eq!(state.log.front().unwrap().line, "entry 3");
    }
}
