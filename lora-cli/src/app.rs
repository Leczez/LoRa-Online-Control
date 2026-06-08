use std::collections::VecDeque;

pub const MAX_LOG_ENTRIES: usize = 1000;

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
}

pub struct App {
    pub log: VecDeque<LogEntry>,
    pub input: String,
    pub should_quit: bool,
    pub addr: u16,
    pub dest: u16,
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
            port_info,
        }
    }

    pub fn config_line(&self) -> String {
        format!("{}  addr: {}  dest: {}", self.port_info, self.addr, self.dest)
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }
}
