// lora-server/src/punch_buffer.rs
//
// Persistent (disk-backed) buffer of punches, so a network outage between
// this daemon and the remote roc/mip output server never loses data — a
// punch is written here the moment it's decoded (from the local SI reader
// or a remote LoRa node), independent of whether or when it can be pushed
// onward. A background pusher (see pusher.rs) drains unsent rows whenever
// the output server is reachable.

use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub struct BufferedPunch {
    pub id: i64,
    pub card_id: u32,
    pub station: u8,
    pub time_s: u32,
    /// Where this punch came from: "local" (this Pi's own SI reader) or the
    /// LoRa source address (as a string) for a remote field node.
    pub source: String,
}

pub struct PunchBuffer {
    conn: Mutex<Connection>,
}

impl PunchBuffer {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS punches (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                card_id  INTEGER NOT NULL,
                station  INTEGER NOT NULL,
                time_s   INTEGER NOT NULL,
                source   TEXT NOT NULL,
                sent     INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Records one punch. Returns its assigned row id.
    pub fn record(&self, card_id: u32, station: u8, time_s: u32, source: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO punches (card_id, station, time_s, source) VALUES (?1, ?2, ?3, ?4)",
            (card_id, station, time_s, source),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All punches not yet successfully pushed to the output server, oldest first.
    pub fn unsent(&self) -> Result<Vec<BufferedPunch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, card_id, station, time_s, source FROM punches WHERE sent = 0 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BufferedPunch {
                id: row.get(0)?,
                card_id: row.get(1)?,
                station: row.get(2)?,
                time_s: row.get(3)?,
                source: row.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn mark_sent(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE punches SET sent = 1 WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_read_unsent() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(12345, 33, 36070, "local").unwrap();
        buf.record(12345, 50, 37300, "local").unwrap();

        let unsent = buf.unsent().unwrap();
        assert_eq!(unsent.len(), 2);
        assert_eq!(unsent[0].card_id, 12345);
        assert_eq!(unsent[0].station, 33);
        assert_eq!(unsent[1].station, 50);
    }

    #[test]
    fn test_mark_sent_removes_from_unsent() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        let id = buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 2, 200, "192.168.1.5").unwrap();

        buf.mark_sent(id).unwrap();

        let unsent = buf.unsent().unwrap();
        assert_eq!(unsent.len(), 1);
        assert_eq!(unsent[0].card_id, 2);
    }

    #[test]
    fn test_unsent_ordered_oldest_first() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 1, 200, "local").unwrap();
        buf.record(3, 1, 300, "local").unwrap();

        let ids: Vec<u32> = buf.unsent().unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
