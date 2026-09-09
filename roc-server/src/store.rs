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
        Ok(rows
            .filter_map(|r| r.inspect_err(|e| log::error!("corrupt punch row skipped: {e}")).ok())
            .collect())
    }

    /// Today's date (`YYYY-MM-DD`, server-local per SQLite's own `date('now')`)
    /// for prefixing the ROC format's timestamp column — see
    /// roc.rs::render_roc_text for why only the date, not a per-punch
    /// lookup: the actual time-of-day in that column has to come from each
    /// punch's own `time_s` (the real SI punch time), not from anything
    /// stored per-row here. One query per response, not per punch, since
    /// the date is the same for the whole batch.
    pub fn today(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT date('now')", [], |row| row.get(0)).map_err(Into::into)
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

    /// A row that can't deserialize (here: a station value that overflows
    /// u8, inserted via raw SQL to bypass record()'s type-safe API) must be
    /// skipped, not panic the whole since() call — surrounding valid rows
    /// still come back. This is what feeds MEOS via /mip and /roc, so a
    /// corrupt row silently taking the rest of the batch down with it would
    /// mean every later punch in that poll also vanishes from MEOS's view.
    #[test]
    fn test_since_skips_corrupt_row_without_losing_other_rows() {
        let store = Store::open(":memory:").unwrap();
        store.record(1, 1, 100, "local").unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO punches (card_id, station, time_s, source) VALUES (?1, ?2, ?3, ?4)",
                (2, 99_999_i64, 200, "local"),
            )
            .unwrap();
        }
        store.record(3, 3, 300, "local").unwrap();

        let card_ids: Vec<u32> = store.since(0).unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(card_ids, vec![1, 3]);
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
