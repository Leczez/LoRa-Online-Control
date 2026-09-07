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
        Ok(rows
            .filter_map(|r| r.inspect_err(|e| log::error!("corrupt punch row skipped: {e}")).ok())
            .collect())
    }

    /// Unsent punches this node itself should transmit — its own local SI
    /// reader (source == "local") plus any operator-triggered test punches
    /// (source == "test", see the TESTPUNCH command in backend.rs, reachable
    /// via POST /testpunch — web.rs),
    /// oldest first. Test punches deliberately flow through the exact same
    /// send/retry/ack path as a real one — that's the point, verifying the
    /// real pipeline — but keep a distinct `source` tag rather than being
    /// recorded as "local" outright, so they stay identifiable later (e.g.
    /// auditing the buffer, or filtering them out of real event data)
    /// instead of being indistinguishable from a genuine card tap. Distinct
    /// from `unsent()`, which also includes remote-sourced punches relevant
    /// to the HTTP push path but not to what this node itself needs to
    /// transmit.
    pub fn unsent_local(&self) -> Result<Vec<BufferedPunch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, card_id, station, time_s, source FROM punches WHERE sent = 0 AND source IN ('local', 'test') ORDER BY id ASC",
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
        Ok(rows
            .filter_map(|r| r.inspect_err(|e| log::error!("corrupt punch row skipped: {e}")).ok())
            .collect())
    }

    pub fn mark_sent(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE punches SET sent = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Permanently abandons one unsent local/test punch — a genuine delete,
    /// not mark_sent, since it was never actually delivered and recording it
    /// as "sent" would misrepresent that. Scoped to source IN ('local',
    /// 'test') and sent = 0, same as unsent_local(), so this can't touch a
    /// remote-sourced row still queued for the roc-server push (see
    /// unsent()) or a row that already went out. Returns whether a row was
    /// actually deleted, so the caller (run_daemon_loop) can tell "cleared"
    /// from "no such unsent local punch".
    pub fn clear_local_unsent(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "DELETE FROM punches WHERE id = ?1 AND sent = 0 AND source IN ('local', 'test')",
            [id],
        )?;
        Ok(affected > 0)
    }

    /// Abandons every unsent local/test punch — the blunt "unstick
    /// everything" version of clear_local_unsent. Returns the number of rows
    /// deleted, so the caller can report exactly how many were dropped.
    pub fn clear_all_local_unsent(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute("DELETE FROM punches WHERE sent = 0 AND source IN ('local', 'test')", [])?;
        Ok(affected)
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
    fn test_clear_local_unsent_removes_only_the_named_row() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        let id1 = buf.record(1, 1, 100, "local").unwrap();
        let id2 = buf.record(2, 2, 200, "test").unwrap();

        assert!(buf.clear_local_unsent(id1).unwrap());
        let remaining: Vec<i64> = buf.unsent().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(remaining, vec![id2]);
    }

    #[test]
    fn test_clear_local_unsent_leaves_remote_and_already_sent_rows_alone() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        let remote_id = buf.record(1, 1, 100, "192.168.1.5").unwrap();
        let sent_id = buf.record(2, 2, 200, "local").unwrap();
        buf.mark_sent(sent_id).unwrap();

        assert!(!buf.clear_local_unsent(remote_id).unwrap());
        assert!(!buf.clear_local_unsent(sent_id).unwrap());
        assert_eq!(buf.unsent().unwrap().iter().map(|p| p.id).collect::<Vec<_>>(), vec![remote_id]);
    }

    #[test]
    fn test_clear_local_unsent_reports_false_for_unknown_id() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        assert!(!buf.clear_local_unsent(999).unwrap());
    }

    #[test]
    fn test_clear_all_local_unsent_only_touches_local_and_test() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 2, 200, "test").unwrap();
        let remote_id = buf.record(3, 3, 300, "192.168.1.5").unwrap();

        let cleared = buf.clear_all_local_unsent().unwrap();
        assert_eq!(cleared, 2);
        assert_eq!(buf.unsent().unwrap().iter().map(|p| p.id).collect::<Vec<_>>(), vec![remote_id]);
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

    /// A row that can't deserialize (here: a station value that overflows
    /// u8, inserted via raw SQL to bypass record()'s type-safe API) must be
    /// skipped, not panic the whole unsent() call — surrounding valid rows
    /// still come back. The skip itself is logged (see unsent()'s
    /// inspect_err), which isn't asserted here, only that it doesn't take
    /// the rest of the batch down with it.
    #[test]
    fn test_unsent_skips_corrupt_row_without_losing_other_rows() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(1, 1, 100, "local").unwrap();
        {
            let conn = buf.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO punches (card_id, station, time_s, source) VALUES (?1, ?2, ?3, ?4)",
                (2, 99_999_i64, 200, "local"),
            )
            .unwrap();
        }
        buf.record(3, 3, 300, "local").unwrap();

        let card_ids: Vec<u32> = buf.unsent().unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(card_ids, vec![1, 3]);
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

    #[test]
    fn test_unsent_local_excludes_remote_sourced_punches() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 2, 200, "192.168.1.5").unwrap();
        buf.record(3, 3, 300, "local").unwrap();

        let ids: Vec<u32> = buf.unsent_local().unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    /// TESTPUNCH (backend.rs) records with source="test" specifically so it
    /// stays identifiable from a genuine card tap, but it still needs to
    /// flow through the same send pipeline as "local" — this is the query
    /// that pipeline reads from.
    #[test]
    fn test_unsent_local_includes_test_sourced_punches() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 2, 200, "test").unwrap();
        buf.record(3, 3, 300, "192.168.1.5").unwrap();

        let ids: Vec<u32> = buf.unsent_local().unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn test_unsent_local_excludes_already_sent() {
        let buf = PunchBuffer::open(":memory:").unwrap();
        let id = buf.record(1, 1, 100, "local").unwrap();
        buf.record(2, 2, 200, "local").unwrap();
        buf.mark_sent(id).unwrap();

        let ids: Vec<u32> = buf.unsent_local().unwrap().iter().map(|p| p.card_id).collect();
        assert_eq!(ids, vec![2]);
    }
}
