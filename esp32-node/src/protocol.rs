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

/// Wraps `radio.send`, prepending the shared deployment network ID (see
/// docs/protocols/lora_online_control_protocol.md, "Network Identification")
/// — must match `lora-server`'s `NetworkFilteredRadio::send` exactly, since
/// that's the same envelope on the other end.
pub fn send_framed<R: sx127x::LoraRadio>(
    radio: &mut R, dest: u16, payload: &[u8], network_id: &str,
) -> Result<(), R::Error> {
    let mut framed = std::vec::Vec::with_capacity(network_id.len() + 1 + payload.len());
    framed.extend_from_slice(network_id.as_bytes());
    framed.push(b' ');
    framed.extend_from_slice(payload);
    radio.send(dest, &framed)
}

/// Wraps `radio.receive`, stripping and validating the network ID before
/// handing back the inner payload as a `String` — a mismatched or missing ID
/// is treated as if nothing was received, not misparsed as one of our own
/// malformed frames. Mirrors `lora-server`'s `NetworkFilteredRadio::receive`.
pub fn receive_framed<R: sx127x::LoraRadio>(
    radio: &mut R, network_id: &str,
) -> Result<Option<(u16, Option<i16>, String)>, R::Error> {
    let Some(pkt) = radio.receive()? else { return Ok(None) };
    let Ok(text) = core::str::from_utf8(&pkt.payload) else { return Ok(None) };
    let Some(rest) = text.strip_prefix(network_id).and_then(|s| s.strip_prefix(' ')) else {
        return Ok(None);
    };
    Ok(Some((pkt.src_addr, pkt.rssi, rest.to_string())))
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
}
