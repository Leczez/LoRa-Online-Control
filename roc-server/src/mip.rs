// roc-server/src/mip.rs
//
// MeOS **Input** Protocol (MIP — verified against real MeOS source, not
// "Info" as an earlier version of this comment assumed) output: MEOS polls
// us with a LastId it has already consumed, we hand back everything newer
// as `<MIPData>` XML. Punch times are tenths of a second since local
// midnight (matches MEOS's default timeConstSecond==10 — see
// OnlineInput::processPunches' transformTime in onlineinput.cpp, which is a
// no-op in that case).
//
// Verified directly against melinsoftware/meos's actual C++ source
// (onlineinput.cpp), not documentation or recollection:
//   - <MIPData lastid="..."> — matches res.getAttrib("lastid")
//   - <p card="..." code="..." time="..."/> — matches
//     punches[k].getObjectInt("card"/"code"/"time") exactly. `code`, not
//     `control` — an earlier version of this file used `control`, which
//     MEOS's parser doesn't read at all: it'd default to 0, fail
//     `if (code <= 0)`, and silently drop every punch. Confirmed by reading
//     OnlineInput::processPunches directly, not guessed.
//   - `sno` (start number/bib) and `type` (start/finish/check) are both
//     optional and intentionally omitted: without `sno`, MEOS looks the
//     runner up by SI card number instead (oe.getRunnerByCardNo), which is
//     exactly what a raw SI punch should do; without `type`, the numeric
//     `code` is passed through MEOS's own configurable control mapping
//     rather than forced into a hardcoded start/finish/check meaning.

use crate::store::StoredPunch;

pub fn render_mip_xml(last_id: i64, punches: &[StoredPunch]) -> String {
    let mut out = format!("<MIPData lastid=\"{}\">\n", last_id);
    for p in punches {
        out.push_str(&format!(
            "  <p card=\"{}\" code=\"{}\" time=\"{}\"/>\n",
            p.card_id,
            p.station,
            p.time_s * 10
        ));
    }
    out.push_str("</MIPData>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_empty() {
        let xml = render_mip_xml(0, &[]);
        assert_eq!(xml, "<MIPData lastid=\"0\">\n</MIPData>\n");
    }

    #[test]
    fn test_render_punches_uses_tenths_of_second() {
        let punches = vec![StoredPunch { id: 5, card_id: 123456, station: 31, time_s: 36070 }];
        let xml = render_mip_xml(5, &punches);
        assert!(xml.contains("lastid=\"5\""));
        assert!(xml.contains("card=\"123456\""));
        assert!(xml.contains("code=\"31\""));
        assert!(xml.contains("time=\"360700\""));
    }

    /// Regression guard: MEOS's real parser (OnlineInput::processPunches in
    /// onlineinput.cpp) reads the control number via getObjectInt("code"),
    /// not "control" — an earlier version of this file got this wrong,
    /// which would silently fail every punch's `if (code <= 0)` check and
    /// drop it. Confirmed by reading MEOS's actual source, not assumed.
    #[test]
    fn test_render_does_not_regress_to_wrong_attribute_name() {
        let punches = vec![StoredPunch { id: 1, card_id: 1, station: 31, time_s: 0 }];
        let xml = render_mip_xml(1, &punches);
        assert!(!xml.contains("control="), "MEOS's parser doesn't read a `control` attribute at all");
    }

    #[test]
    fn test_render_multiple_punches_in_order() {
        let punches = vec![
            StoredPunch { id: 1, card_id: 1, station: 1, time_s: 100 },
            StoredPunch { id: 2, card_id: 2, station: 2, time_s: 200 },
        ];
        let xml = render_mip_xml(2, &punches);
        let first = xml.find("card=\"1\"").unwrap();
        let second = xml.find("card=\"2\"").unwrap();
        assert!(first < second);
    }
}
