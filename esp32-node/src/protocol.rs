//! Minimal port of `lora-server/src/protocol.rs`'s `Frame` — just the
//! variants esp32-node itself needs to understand. This node only
//! originates punches and doesn't relay Command/Ack frames for other nodes,
//! so the full `Frame` enum isn't needed here — just enough to recognize the
//! ack for our own outstanding send, and (as of the version-query addition)
//! to reply to a version query addressed to us and to announce our own
//! version once on boot. See docs/protocols/lora_online_control_protocol.md,
//! "Punch Delivery" and "Version Reporting".

/// Parses a `PACK <node> <card_id>` wire payload — must match
/// `lora-server`'s `Frame::PunchAck::encode()` format exactly, since that's
/// what actually produces this on the wire.
pub fn parse_punch_ack(s: &str) -> Option<(u16, u32)> {
    let rest = s.strip_prefix("PACK ")?;
    let mut parts = rest.splitn(2, ' ');
    let node: u16 = parts.next()?.parse().ok()?;
    let card_id: u32 = parts.next()?.parse().ok()?;
    Some((node, card_id))
}

/// Parses a `VQUERY <target>` wire payload — must match `lora-server`'s
/// `Frame::VersionQuery::encode()` format exactly.
pub fn parse_version_query(s: &str) -> Option<u16> {
    let rest = s.strip_prefix("VQUERY ")?;
    rest.trim().parse().ok()
}

/// Encodes a `VERSION <origin> <version>` wire payload — must match
/// `lora-server`'s `Frame::VersionReport::encode()` format exactly, since
/// that's what parses it on the other end. Used both for the unprompted
/// boot announcement and any reply to a `VQUERY`.
pub fn encode_version_report(origin: u16, version: &str) -> String {
    std::format!("VERSION {} {}", origin, version)
}

/// Parses a `CACK <target>` wire payload — must match `lora-server`'s
/// `Frame::ConfigAck::encode()` format exactly. Sent by the base station in
/// reply to every `VersionReport` it receives (including the unprompted
/// boot announcement below) — this node's proof that its current LoRa mode
/// is actually reaching the base station. See main.rs's boot-verification
/// logic and docs/protocols/lora_online_control_protocol.md, "RF
/// Parameters".
pub fn parse_config_ack(s: &str) -> Option<u16> {
    let rest = s.strip_prefix("CACK ")?;
    rest.trim().parse().ok()
}

/// Wraps `radio.receive`, validating the payload as UTF-8 and packaging it
/// with the sender/RSSI into the tuple `main.rs`'s loop consumes — a
/// malformed (non-UTF-8) payload is treated as if nothing was received, not
/// misparsed as one of our own malformed frames. No longer strips a shared
/// deployment network ID (removed — the radio's own sync word already
/// guards against cross-talk with another deployment nearby, for free, at
/// the hardware level, checked during preamble detection before the chip
/// even demodulates a mismatched packet — see
/// docs/protocols/lora_online_control_protocol.md, "Network Identification"
/// and NodeConfig::sync_word).
pub fn receive_text<R: sx127x::LoraRadio>(
    radio: &mut R,
) -> Result<Option<(u16, Option<i16>, String)>, R::Error> {
    let Some(pkt) = radio.receive()? else { return Ok(None) };
    let Ok(text) = core::str::from_utf8(&pkt.payload) else { return Ok(None) };
    Ok(Some((pkt.src_addr, pkt.rssi, text.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_punch_ack() {
        assert_eq!(parse_punch_ack("PACK 12 123456"), Some((12, 123456)));
    }

    #[test]
    fn test_parse_punch_ack_rejects_garbage() {
        assert_eq!(parse_punch_ack("PACK notanumber 123"), None);
        assert_eq!(parse_punch_ack("HB"), None);
        assert_eq!(parse_punch_ack("PACK 12"), None);
    }

    #[test]
    fn test_parse_version_query() {
        assert_eq!(parse_version_query("VQUERY 10"), Some(10));
    }

    #[test]
    fn test_parse_version_query_rejects_garbage() {
        assert_eq!(parse_version_query("VQUERY notanumber"), None);
        assert_eq!(parse_version_query("HB"), None);
    }

    #[test]
    fn test_encode_version_report() {
        assert_eq!(encode_version_report(10, "0.1.0+a1b2c3d4"), "VERSION 10 0.1.0+a1b2c3d4");
    }

    #[test]
    fn test_parse_config_ack() {
        assert_eq!(parse_config_ack("CACK 10"), Some(10));
    }

    #[test]
    fn test_parse_config_ack_rejects_garbage() {
        assert_eq!(parse_config_ack("CACK notanumber"), None);
        assert_eq!(parse_config_ack("HB"), None);
    }
}
