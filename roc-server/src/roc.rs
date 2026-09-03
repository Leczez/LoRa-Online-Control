// roc-server/src/roc.rs
//
// ROC (Radio Online Control) output: a simple polled, semicolon-delimited
// text format — `id;control;card;timestamp` per line, oldest first.
//
// NOTE: mirrors the shape of the real roc.olresultat.se polling format as
// best recalled during design; verify against a real client before relying
// on this in a live event.

use crate::store::StoredPunch;

pub fn render_roc_text(punches: &[StoredPunch], timestamps: &[String]) -> String {
    punches
        .iter()
        .zip(timestamps.iter())
        .map(|(p, ts)| format!("{};{};{};{}", p.id, p.station, p.card_id, ts))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_empty() {
        assert_eq!(render_roc_text(&[], &[]), "");
    }

    #[test]
    fn test_render_single_punch() {
        let punches = vec![StoredPunch { id: 7, card_id: 123456, station: 31, time_s: 36070 }];
        let timestamps = vec!["2026-08-31 10:01:10".to_string()];
        let text = render_roc_text(&punches, &timestamps);
        assert_eq!(text, "7;31;123456;2026-08-31 10:01:10");
    }

    #[test]
    fn test_render_multiple_punches_newline_separated() {
        let punches = vec![
            StoredPunch { id: 1, card_id: 1, station: 1, time_s: 100 },
            StoredPunch { id: 2, card_id: 2, station: 2, time_s: 200 },
        ];
        let timestamps = vec!["ts1".to_string(), "ts2".to_string()];
        let text = render_roc_text(&punches, &timestamps);
        assert_eq!(text, "1;1;1;ts1\n2;2;2;ts2");
    }
}
