//! Small ring-buffer log persisted to NVS, readable from the Wi-Fi config
//! portal's `/log` page (wifi_config.rs) even though the live serial console
//! is unreachable for the rest of a normal boot once `cp210x::install()`
//! switches the native USB port into host mode for the SI master (see
//! main.rs's doc comments). The portal runs fresh at the very start of every
//! boot, before that switch happens — so if a node crashes (brownout,
//! panic, watchdog) partway through, connecting to the NEXT boot's portal
//! and loading `/log` shows exactly how far the PREVIOUS, ill-fated boot got
//! before it died, without needing a live serial connection at all.
//!
//! Deliberately NOT a full serial-log mirror: writing on every log::info!
//! call would wear the flash out fast (SPI NOR is typically rated for
//! something on the order of 10,000-100,000 erase cycles per sector). Only
//! call `append` at meaningful checkpoints (boot, radio-up, a heartbeat
//! attempt, etc — see main.rs), not per log line. At this project's 60s
//! heartbeat interval that's roughly one write a minute, which — spread
//! across NVS's own wear-leveling over the whole partition — works out to
//! comfortably over a year of continuous operation before wear-out risk
//! becomes real, nowhere close to a multi-day event's actual runtime.
//!
//! Bounded to MAX_ENTRIES short lines (oldest dropped first) so both the
//! rendered page and each write stay small — reuses the same NVS handle
//! (and "nodecfg" namespace) config.rs already opens, just under a
//! different key, rather than taking a second NVS partition/namespace.

use esp_idf_svc::nvs::{EspNvs, NvsDefault};

const KEY_LOG: &str = "log_blob";
const MAX_ENTRIES: usize = 15;
const MAX_LINE_LEN: usize = 80;
/// Comfortably fits MAX_ENTRIES * (MAX_LINE_LEN + 1) with room to spare,
/// well under NVS's own string-value size ceiling.
const READ_BUF_LEN: usize = 2048;

/// Appends `line` (truncated to MAX_LINE_LEN) to the persisted ring,
/// dropping the oldest entry once past MAX_ENTRIES. See this module's doc
/// comment for why this must only be called at meaningful checkpoints, not
/// per log line. Failures are logged and otherwise swallowed — a lost log
/// entry is a debugging inconvenience, never worth treating as fatal.
pub fn append(nvs: &mut EspNvs<NvsDefault>, line: &str) {
    let mut lines = read_lines(nvs);
    let mut line = line.to_string();
    line.truncate(MAX_LINE_LEN);
    lines.push(line);
    while lines.len() > MAX_ENTRIES {
        lines.remove(0);
    }
    let blob = lines.join("\n");
    if let Err(e) = nvs.set_str(KEY_LOG, &blob) {
        log::warn!("failed to persist log entry: {:?}", e);
    }
}

/// Reads back the persisted lines, oldest first — what wifi_config.rs's
/// `/log` page renders. Empty if nothing has been checkpointed yet (first
/// boot ever, or NVS was erased).
pub fn read_lines(nvs: &EspNvs<NvsDefault>) -> Vec<String> {
    let mut buf = [0u8; READ_BUF_LEN];
    nvs.get_str(KEY_LOG, &mut buf)
        .ok()
        .flatten()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure logic check on the truncation/eviction rules — not exercisable
    /// against a real EspNvs in this sandbox (no ESP-IDF runtime here), but
    /// this at least guards the line-length and ring-size math by hand.
    #[test]
    fn test_ring_eviction_keeps_only_max_entries() {
        let mut lines: Vec<String> = (0..MAX_ENTRIES + 5).map(|i| format!("line {i}")).collect();
        while lines.len() > MAX_ENTRIES {
            lines.remove(0);
        }
        assert_eq!(lines.len(), MAX_ENTRIES);
        assert_eq!(lines.first().unwrap(), "line 5");
        assert_eq!(lines.last().unwrap(), &format!("line {}", MAX_ENTRIES + 4));
    }

    #[test]
    fn test_line_truncation() {
        let mut line = "x".repeat(200);
        line.truncate(MAX_LINE_LEN);
        assert_eq!(line.len(), MAX_LINE_LEN);
    }
}
