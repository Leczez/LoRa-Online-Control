//! Minimal port of `lora-server/src/protocol.rs`'s `Frame::PunchAck` — the
//! only frame type esp32-node needs to understand. This node only
//! originates punches (it never relays Command/Ack frames for other nodes),
//! so the full `Frame` enum isn't needed here — just enough to recognize the
//! ack for our own outstanding send. See
//! docs/protocols/lora_online_control_protocol.md, "Punch Delivery".

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
}
