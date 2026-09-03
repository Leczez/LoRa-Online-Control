// roc-server/src/store.rs
//
// Persisted log of punches received from one or more lora-server daemons.
// This is a separate store from lora-server's own punch_buffer — that one tracks
// "has this been pushed to us yet", this one tracks "has MIP/ROC clients
// already seen this" (its own incrementing id space, its own file).

use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub struct StoredPunch {
    pub id: i64,
    pub card_id: u32,
    pub station: u8,
    pub time_s: u32,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS punches (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                card_id  INTEGER NOT NULL,
                station  INTEGER NOT NULL,
                time_s   INTEGER NOT NULL,
                source   TEXT NOT NULL,
                received_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn record(&self, card_id: u32, station: u8, time_s: u32, source: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO punches (card_id, station, time_s, source) VALUES (?1, ?2, ?3, ?4)",
            (card_id, station, time_s, source),
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// All punches with id strictly greater than `last_id`, oldest first.
    pub fn since(&self, last_id: i64) -> Result<Vec<StoredPunch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, card_id, station, time_s FROM punches WHERE id > ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([last_id], |row| {
            Ok(StoredPunch {
                id: row.get(0)?,
                card_id: row.get(1)?,
                station: row.get(2)?,
                time_s: row.get(3)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Timestamp string (`YYYY-MM-DD HH:MM:SS`) for the ROC format, computed
    /// from the row's received_at rather than a re-derived value, so it's
    /// stable across repeated queries for the same punch.
    pub fn timestamp_of(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT received_at FROM punches WHERE id = ?1")?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_since() {
        let store = Store::open(":memory:").unwrap();
        store.record(111, 33, 36070, "local").unwrap();
        store.record(222, 50, 37300, "192.168.1.5").unwrap();

        let all = store.since(0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].card_id, 111);
        assert_eq!(all[1].card_id, 222);
    }

    #[test]
    fn test_since_only_returns_newer() {
        let store = Store::open(":memory:").unwrap();
        let id1 = store.record(1, 1, 100, "local").unwrap();
        store.record(2, 1, 200, "local").unwrap();

        let newer = store.since(id1).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].card_id, 2);
    }
}
