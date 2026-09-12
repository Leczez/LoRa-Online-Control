//! Minimal port of `lora-server/src/protocol.rs`'s `Frame` — just the
//! variants esp32-node itself needs to understand. This node only
//! originates punches and doesn't relay Command/Ack frames for other nodes,
//! so the full `Frame` enum isn't needed here — just enough to recognize the
//! ack for our own outstanding send, and (as of the version-query addition)
//! to reply to a version query addressed to us and to announce our own
//! version once on boot. See docs/protocols/lora_online_control_protocol.md,
//! "Punch Delivery" and "Version Reporting".
//!
//! PACK is binary (see `parse_punch_ack`); VQUERY/VERSION/CACK stay plain
//! text — see docs/protocols/lora_online_control_protocol.md, "Wire
//! Format", for why only the high-frequency frames (PUNCH/PACK/HB) moved
//! off text.

// Wire tag byte for the binary PunchAck frame — must match lora-server's
// own protocol.rs::TAG_PUNCH_ACK exactly.
const TAG_PUNCH_ACK: u8 = 0x02;
const PUNCH_ACK_LEN: usize = 7;

/// Parses a binary PunchAck payload — must match `lora-server`'s
/// `encode_punch_ack` exactly, since that's what actually produces this on
/// the wire. Wire format: `[tag:1][node:u16][card_id:u32]`, big-endian.
pub fn parse_punch_ack(bytes: &[u8]) -> Option<(u16, u32)> {
    if bytes.len() != PUNCH_ACK_LEN || bytes[0] != TAG_PUNCH_ACK {
        return None;
    }
    let node = u16::from_be_bytes([bytes[1], bytes[2]]);
    let card_id = u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
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

/// Wraps `radio.receive`, packaging the raw payload bytes with the
/// sender/RSSI into the tuple `main.rs`'s loop consumes. Returns raw bytes,
/// not text — unlike before the wire-efficiency pass, a payload can now be
/// genuinely binary (PACK), so validating/converting to UTF-8 has to wait
/// until after the binary shapes have had a chance to match; see main.rs's
/// receive-loop dispatch and `parse_punch_ack` above. No longer strips a
/// shared deployment network ID (removed — the radio's own sync word
/// already guards against cross-talk with another deployment nearby, for
/// free, at the hardware level, checked during preamble detection before
/// the chip even demodulates a mismatched packet — see
/// docs/protocols/lora_online_control_protocol.md, "Network Identification"
/// and NodeConfig::sync_word).
pub fn receive_raw<R: sx127x::LoraRadio>(
    radio: &mut R,
) -> Result<Option<(u16, Option<i16>, Vec<u8>)>, R::Error> {
    let Some(pkt) = radio.receive()? else { return Ok(None) };
    Ok(Some((pkt.src_addr, pkt.rssi, pkt.payload.to_vec())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_punch_ack() {
        // node=12 (0x000C), card_id=123456 (0x0001E240) — matches
        // lora-server's own encode_punch_ack test vector exactly.
        let bytes = [TAG_PUNCH_ACK, 0x00, 0x0C, 0x00, 0x01, 0xE2, 0x40];
        assert_eq!(parse_punch_ack(&bytes), Some((12, 123456)));
    }

    #[test]
    fn test_parse_punch_ack_rejects_wrong_tag_or_length() {
        let mut wrong_tag = [TAG_PUNCH_ACK, 0x00, 0x0C, 0x00, 0x01, 0xE2, 0x40];
        wrong_tag[0] = 0xFF;
        assert_eq!(parse_punch_ack(&wrong_tag), None);
        assert_eq!(parse_punch_ack(b"HB"), None);
        assert_eq!(parse_punch_ack(&[TAG_PUNCH_ACK, 0x00, 0x0C]), None);
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
