// roc-server/src/mip.rs
//
// MeOS Info Protocol (MIP) output: MEOS polls us with a LastId it has
// already consumed, we hand back everything newer as `<MIPData>` XML.
// Punch times are tenths of a second since local midnight, per MIP.
//
// NOTE: field names here (`card`, `control`, `time`) follow the shape
// described in MeOS's own "Info server protocol" documentation as best
// recalled during design; verify against a real MeOS instance's MIP
// requests before relying on this in a live event, since MEOS is picky
// about exact attribute names.

use crate::store::StoredPunch;

pub fn render_mip_xml(last_id: i64, punches: &[StoredPunch]) -> String {
    let mut out = format!("<MIPData lastid=\"{}\">\n", last_id);
    for p in punches {
        out.push_str(&format!(
            "  <p card=\"{}\" control=\"{}\" time=\"{}\"/>\n",
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
        assert!(xml.contains("control=\"31\""));
        assert!(xml.contains("time=\"360700\""));
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
