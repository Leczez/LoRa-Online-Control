use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub const MAX_LOG_ENTRIES: usize = 1000;

/// Node health as seen from this attach session — reset each time lora-tui
/// (re)connects, unlike the daemon's own daemon_state.rs (which the web
/// dashboard reads and which persists for the daemon's whole lifetime).
#[derive(Debug, Clone, Default)]
pub struct NodeStatus {
    pub last_heartbeat: Option<Instant>,
    pub battery_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    /// See daemon_state::NodeStatus::si_present's doc comment — same
    /// semantics, just reset on reconnect like the rest of this struct.
    pub si_present: Option<bool>,
    /// Punches seen from this node since this session attached — not the
    /// daemon-wide lifetime total (see daemon_state::NodeStatus::punch_count
    /// for that; the web dashboard shows it, this is the TUI's own view).
    pub punch_count: u64,
}

#[derive(Debug)]
pub enum LogEntry {
    Rx {
        timestamp: String,
        src_addr: u16,
        payload: String,
        rssi: Option<i16>,
    },
    Tx {
        timestamp: String,
        dest_addr: u16,
        payload: String,
    },
    Heartbeat {
        timestamp: String,
        dest_addr: u16,
    },
    Error {
        timestamp: String,
        message: String,
    },
    SiPunch {
        timestamp: String,
        card_id: u32,
        punches: Vec<(u8, u32)>,  // (station, time_s)
    },
    CmdResult {
        timestamp: String,
        target: u16,
        message: String,
        ok: bool,
    },
    TestPunchResult {
        timestamp: String,
        card_id: u32,
        station: u8,
        time_s: u32,
    },
    ClearPunchResult {
        timestamp: String,
        message: String,
    },
    /// Purely informational, non-error text — currently just /help's output,
    /// one entry per line so each renders on its own row like every other
    /// log entry.
    Info {
        timestamp: String,
        message: String,
    },
    /// A punch this session originated was acked — see
    /// StatusEvent::PunchAckOk's doc comment in backend.rs.
    PunchAcked {
        timestamp: String,
        card_id: u32,
        acked_by: u16,
    },
    /// This session received and applied a Command — see
    /// StatusEvent::CmdApplied's doc comment in backend.rs.
    CmdApplied {
        timestamp: String,
        commander: u16,
        message: String,
    },
}

pub struct App {
    pub log: VecDeque<LogEntry>,
    pub input: String,
    pub should_quit: bool,
    pub addr: u16,
    pub dest: u16,
    pub scroll_offset: usize,
    pub nodes: HashMap<u16, NodeStatus>,
    port_info: String,
}

impl App {
    pub fn new(port_info: String, addr: u16, dest: u16) -> Self {
        Self {
            log: VecDeque::new(),
            input: String::new(),
            should_quit: false,
            addr,
            dest,
            scroll_offset: 0,
            nodes: HashMap::new(),
            port_info,
        }
    }

    pub fn record_heartbeat(&mut self, node: u16, battery: Option<(u8, u16)>, si_present: Option<bool>) {
        let entry = self.nodes.entry(node).or_default();
        entry.last_heartbeat = Some(Instant::now());
        if let Some((pct, mv)) = battery {
            entry.battery_pct = Some(pct);
            entry.battery_mv = Some(mv);
        }
        if let Some(present) = si_present {
            entry.si_present = Some(present);
        }
    }

    pub fn record_punch(&mut self, node: u16) {
        self.nodes.entry(node).or_default().punch_count += 1;
    }

    pub fn config_line(&self) -> String {
        format!("{}  addr: {}  dest: {}", self.port_info, self.addr, self.dest)
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        let at_capacity = self.log.len() >= MAX_LOG_ENTRIES;
        if at_capacity {
            self.log.pop_front();
        }
        self.log.push_back(entry);
        // Keep the viewport pinned when scrolled; when at capacity the pop+push
        // leaves total unchanged so no adjustment is needed.
        if self.scroll_offset > 0 && !at_capacity {
            self.scroll_offset += 1;
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset += 1;
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
}
