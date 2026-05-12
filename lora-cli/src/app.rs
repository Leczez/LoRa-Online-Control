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
    Error {
        timestamp: String,
        message: String,
    },
}

pub struct App {
    pub log: VecDeque<LogEntry>,
    pub input: String,
    pub should_quit: bool,
    pub config_line: String,
}

impl App {
    pub fn new(config_line: String) -> Self {
        Self {
            log: VecDeque::new(),
            input: String::new(),
            should_quit: false,
            config_line,
        }
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }
}
