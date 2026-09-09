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
//   - A leading `<?xml version="1.0" encoding="utf-8"?>` is required, not
//     decorative: xmlparser::read (xmlparser.cpp) unconditionally discards
//     everything up to the *first* '>' in the response, assuming that
//     prefix is the declaration (also how it detects UTF-8 — see
//     checkUTF). Without one, the first '>' is the end of <MIPData
//     lastid="..."> itself, so MEOS discards our own root element's
//     opening tag and the parse breaks. An earlier version of this file
//     omitted it — real bug, not hypothetical, confirmed by reading
//     xmlparser::read directly.

use crate::store::StoredPunch;

pub fn render_mip_xml(last_id: i64, punches: &[StoredPunch]) -> String {
    // Required, not decorative: MEOS's xmlparser::read (xmlparser.cpp)
    // unconditionally reads up to the *first* '>' in the response and
    // discards it, assuming it's an <?xml ...?> declaration (that's also
    // how it detects UTF-8 — see checkUTF, which specifically looks for
    // "<?xml" and "UTF-8" in that first chunk). Without this line, the
    // first '>' in the document is the end of <MIPData lastid="...">
    // itself, so MEOS silently discards our own root element's opening
    // tag and the parse breaks — confirmed by reading xmlparser::read
    // directly, not assumed. /roc never hits this: it's plain CSV, not XML.
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!("<MIPData lastid=\"{}\">\n", last_id));
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
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<MIPData lastid=\"0\">\n</MIPData>\n"
        );
    }

    /// Simulates MEOS's own parser quirk (xmlparser::read in
    /// xmlparser.cpp): it discards everything up to and including the
    /// FIRST '>' character, assuming that prefix is an <?xml ...?>
    /// declaration. Without a real one, that first '>' would be the end of
    /// <MIPData lastid="..."> itself, corrupting the parse. Proves the
    /// declaration survives that exact discard and the root element's
    /// opening tag is still intact in what's left afterward.
    #[test]
    fn test_survives_meos_first_gt_discard() {
        let punches = vec![StoredPunch { id: 1, card_id: 42, station: 31, time_s: 100 }];
        let xml = render_mip_xml(1, &punches);
        let first_gt = xml.find('>').expect("no '>' in output at all");
        let after_discard = &xml[first_gt + 1..];
        assert!(
            after_discard.trim_start().starts_with("<MIPData"),
            "MEOS would discard the <MIPData> opening tag itself: {after_discard:?}"
        );
        assert!(after_discard.contains("</MIPData>"), "closing tag missing after discard: {after_discard:?}");
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
