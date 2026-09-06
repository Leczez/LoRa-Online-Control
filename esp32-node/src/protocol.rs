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
}
