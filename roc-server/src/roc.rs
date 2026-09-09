// roc-server/src/roc.rs
//
// ROC (Radio Online Control) output: a simple polled, semicolon-delimited
// text format — `id;control;card;timestamp` per line, oldest first.
//
// Verified directly against melinsoftware/meos's actual C++ source
// (OnlineInput::processPunches(oe, list<vector<wstring>> &rocData) in
// onlineinput.cpp), not documentation or recollection: MEOS's csvparser
// splits on `;` (confirmed by a sibling comment in that file noting the
// SportIdent-Center format "can't use csv.parse as it expects semi-colon as
// separator"), requires exactly 4 fields per line, and reads them
// positionally as punchId/code/card/timeS — `timeS = line[3].substr(11)`,
// i.e. it expects the 4th field to be `"YYYY-MM-DD HH:MM:SS"` and strips
// the leading 11-character date prefix itself, so whatever's in that date
// prefix is never read at all — only the time-of-day matters. Field order
// and the timestamp format below match this exactly. MEOS's own request
// query param is `lastId` (confirmed in the same file) — matched in main.rs.
//
// The time-of-day itself MUST come from each punch's own `time_s` (seconds
// since local midnight — the real SI punch time, same source mip.rs uses),
// not from a "when did roc-server happen to receive this" wall-clock
// value: an earlier version of this file took a `timestamps: &[String]`
// parameter built from the DB row's `received_at`, which could legitimately
// differ from the real punch time by however long radio retries/buffering/
// the push interval took — and did, live: MIP and ROC showed different
// times in MEOS for the exact same punch. Fixed by deriving the
// time-of-day from `time_s` directly, same as MIP; `date` (still needed
// only to satisfy the 11-char prefix MEOS strips) is the one piece that
// still comes from outside a punch's own data, since `time_s` alone has no
// date component and MEOS never reads it anyway.

use crate::store::StoredPunch;

pub fn render_roc_text(punches: &[StoredPunch], date: &str) -> String {
    punches
        .iter()
        .map(|p| {
            let h = p.time_s / 3600;
            let m = (p.time_s % 3600) / 60;
            let s = p.time_s % 60;
            format!("{};{};{};{date} {h:02}:{m:02}:{s:02}", p.id, p.station, p.card_id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_empty() {
        assert_eq!(render_roc_text(&[], "2026-08-31"), "");
    }

    #[test]
    fn test_render_single_punch() {
        let punches = vec![StoredPunch { id: 7, card_id: 123456, station: 31, time_s: 36070 }];
        let text = render_roc_text(&punches, "2026-08-31");
        // 36070s = 10h01m10s — matches mip.rs's own tenths-of-a-second
        // encoding of the same time_s (360700), just formatted as HH:MM:SS.
        assert_eq!(text, "7;31;123456;2026-08-31 10:01:10");
    }

    #[test]
    fn test_render_multiple_punches_newline_separated() {
        let punches = vec![
            StoredPunch { id: 1, card_id: 1, station: 1, time_s: 100 },
            StoredPunch { id: 2, card_id: 2, station: 2, time_s: 200 },
        ];
        let text = render_roc_text(&punches, "2026-08-31");
        assert_eq!(text, "1;1;1;2026-08-31 00:01:40\n2;2;2;2026-08-31 00:03:20");
    }

    /// Regression guard for the actual bug this whole rework fixes: MIP and
    /// ROC must report the exact same time-of-day for the same punch,
    /// derived from the same time_s — not two different sources (an
    /// earlier version of render_roc_text took a separately-fetched
    /// "received_at" wall-clock timestamp instead, which could legitimately
    /// disagree with the real punch time and did, live, in MEOS).
    #[test]
    fn test_time_of_day_matches_mips_encoding_of_the_same_time_s() {
        let time_s = 36070u32;
        let punches = vec![StoredPunch { id: 1, card_id: 1, station: 1, time_s }];
        let roc_text = render_roc_text(&punches, "2026-08-31");

        let mip_xml = crate::mip::render_mip_xml(1, &punches);
        let mip_tenths: u32 = mip_xml
            .split("time=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.parse().ok())
            .unwrap();
        let mip_seconds = mip_tenths / 10;
        let expected_hms = format!("{:02}:{:02}:{:02}", mip_seconds / 3600, (mip_seconds % 3600) / 60, mip_seconds % 60);

        assert!(roc_text.ends_with(&expected_hms), "roc_text was: {roc_text}, expected time-of-day {expected_hms}");
    }
}
