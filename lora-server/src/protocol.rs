// lora-server/src/protocol.rs
//
// Wire format for the control-plane frames layered on top of the existing
// binary PUNCH/HB payloads (see sportident.rs and backend.rs) and the
// binary PunchAck below: a downlink `CMD` frame lets the base station
// change a limited set of settings on a specific node, and an uplink `ACK`
// frame confirms it landed. Scope is deliberately narrow — only settings
// that can't strand a node if misapplied (see the protocol doc's "Command
// Packets" section) get a `Setting` variant here.
//
// `Frame` itself stays plain ASCII text — every variant here is sent at
// most a handful of times per node per session (a boot announcement, an
// occasional settings change), so the airtime cost of decimal-text framing
// is negligible in aggregate, and staying human-readable in lora-tui's raw
// traffic log or a serial console is worth more than the few bytes binary
// framing would save. `encode_punch_ack`/`parse_punch_ack` below are the
// one exception — `PunchAck` used to be a `Frame` variant too, but it's
// sent once per punch, so it moved to packed binary alongside PUNCH/HB (see
// docs/protocols/lora_online_control_protocol.md, "Wire Format").

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    HeartbeatIntervalSecs(u32),
}

impl Setting {
    pub fn encode(&self) -> String {
        match self {
            Setting::HeartbeatIntervalSecs(v) => format!("hb_interval={}", v),
        }
    }

    pub fn parse(s: &str) -> Option<Setting> {
        let (key, val) = s.split_once('=')?;
        match key {
            "hb_interval" => Some(Setting::HeartbeatIntervalSecs(val.parse().ok()?)),
            _ => None,
        }
    }
}

// No `Copy` — VersionReport's version field is a String, unlike every other
// variant here, which were all plain integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Downlink: `commander` -> node at `target`, asking it to change
    /// `setting`. `commander` travels in the payload (not just inferred from
    /// the radio header) so that once this is relayed, the resulting `Ack`
    /// can be forwarded back to the right place rather than just to
    /// whichever relay last touched it.
    Command { target: u16, commander: u16, setting: Setting },
    /// Uplink: node at `origin` confirming it applied `setting`, addressed
    /// back to `commander` (echoed from the `Command` that prompted it) so
    /// a relay forwarding this ack knows where it's ultimately headed.
    Ack { origin: u16, commander: u16, setting: Setting },
    /// Downlink: asks node `target` to report its firmware version.
    /// Distinct from `Command`/`Ack` (which apply a `Setting` the commander
    /// already knows the value of) since a version's value is exactly what
    /// the commander doesn't have — nothing to echo back and confirm, just
    /// something to report. No `commander`/relay-routing field, same
    /// accepted limitation as `HB` (see its own doc comment): this is a
    /// one-off diagnostic query, not safety-critical config, so relay
    /// support isn't worth the complexity yet.
    VersionQuery { target: u16 },
    /// Uplink: node `origin`'s firmware version. Sent both in reply to a
    /// `VersionQuery` and once, unprompted, right after booting (see
    /// esp32-node/src/main.rs) — same wire shape either way, so parsing
    /// doesn't need to care which prompted it.
    VersionReport { origin: u16, version: String },
    /// Downlink: sent to `target` for every `VersionReport` this base
    /// station receives from it, whether that report was prompted by a
    /// `VersionQuery` or was the node's own unprompted boot announcement —
    /// doesn't distinguish which, since both cases are equally valid proof
    /// the node's current LoRa mode (freq/SF/BW/CR/sync word) is actually
    /// reaching this base station. A freshly booted node uses this as its
    /// "my current config was heard" signal during a short post-boot
    /// window, reverting to a known-good default if none arrives — see
    /// esp32-node/src/main.rs and docs/protocols/lora_online_control_protocol.md,
    /// "RF Parameters". No `commander`/relay-routing field — same accepted
    /// limitation as `VersionReport`/`HB`, not relayed.
    ConfigAck { target: u16 },
}

impl Frame {
    pub fn encode(&self) -> String {
        match self {
            Frame::Command { target, commander, setting } => format!("CMD {} {} {}", target, commander, setting.encode()),
            Frame::Ack { origin, commander, setting } => format!("ACK {} {} {}", origin, commander, setting.encode()),
            Frame::VersionQuery { target } => format!("VQUERY {}", target),
            Frame::VersionReport { origin, version } => format!("VERSION {} {}", origin, version),
            Frame::ConfigAck { target } => format!("CACK {}", target),
        }
    }

    pub fn parse(s: &str) -> Option<Frame> {
        if let Some(rest) = s.strip_prefix("CMD ") {
            let mut parts = rest.splitn(3, ' ');
            let target: u16 = parts.next()?.parse().ok()?;
            let commander: u16 = parts.next()?.parse().ok()?;
            let setting = Setting::parse(parts.next()?)?;
            return Some(Frame::Command { target, commander, setting });
        }
        if let Some(rest) = s.strip_prefix("ACK ") {
            let mut parts = rest.splitn(3, ' ');
            let origin: u16 = parts.next()?.parse().ok()?;
            let commander: u16 = parts.next()?.parse().ok()?;
            let setting = Setting::parse(parts.next()?)?;
            return Some(Frame::Ack { origin, commander, setting });
        }
        if let Some(rest) = s.strip_prefix("VQUERY ") {
            let target: u16 = rest.trim().parse().ok()?;
            return Some(Frame::VersionQuery { target });
        }
        if let Some(rest) = s.strip_prefix("VERSION ") {
            let mut parts = rest.splitn(2, ' ');
            let origin: u16 = parts.next()?.parse().ok()?;
            let version = parts.next()?.to_string();
            if version.is_empty() {
                return None;
            }
            return Some(Frame::VersionReport { origin, version });
        }
        if let Some(rest) = s.strip_prefix("CACK ") {
            let target: u16 = rest.trim().parse().ok()?;
            return Some(Frame::ConfigAck { target });
        }
        None
    }
}

// Wire tag byte for the binary PunchAck frame — see sportident.rs's
// TAG_PUNCH doc comment for the full tag registry.
const TAG_PUNCH_ACK: u8 = 0x02;
// tag + node(2) + card_id(4), always this exact length.
const PUNCH_ACK_LEN: usize = 7;

/// Confirms to `node` that its punch for `card_id` was received. A node
/// holds its next punch (stop-and-wait, see the protocol doc) until this
/// arrives or a retry timeout elapses, so only one punch is ever
/// unacknowledged at a time per node — `card_id` alone is enough to
/// disambiguate since there's never more than one outstanding. Binary, not
/// part of the `Frame` enum above — see this module's doc comment for why:
/// sent once per punch, same traffic volume as PUNCH itself.
///
/// Wire format: `[tag:1][node:u16][card_id:u32]`, big-endian.
pub fn encode_punch_ack(node: u16, card_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PUNCH_ACK_LEN);
    buf.push(TAG_PUNCH_ACK);
    buf.extend_from_slice(&node.to_be_bytes());
    buf.extend_from_slice(&card_id.to_be_bytes());
    buf
}

/// Inverse of `encode_punch_ack`.
pub fn parse_punch_ack(bytes: &[u8]) -> Option<(u16, u32)> {
    if bytes.len() != PUNCH_ACK_LEN || bytes[0] != TAG_PUNCH_ACK {
        return None;
    }
    let node = u16::from_be_bytes([bytes[1], bytes[2]]);
    let card_id = u32::from_be_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
    Some((node, card_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_round_trips() {
        let frame = Frame::Command { target: 5, commander: 1, setting: Setting::HeartbeatIntervalSecs(30) };
        let encoded = frame.encode();
        assert_eq!(encoded, "CMD 5 1 hb_interval=30");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_ack_round_trips() {
        let frame = Frame::Ack { origin: 7, commander: 1, setting: Setting::HeartbeatIntervalSecs(45) };
        let encoded = frame.encode();
        assert_eq!(encoded, "ACK 7 1 hb_interval=45");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_punch_ack_round_trips() {
        let encoded = encode_punch_ack(12, 123456);
        assert_eq!(encoded, vec![TAG_PUNCH_ACK, 0x00, 0x0C, 0x00, 0x01, 0xE2, 0x40]);
        assert_eq!(parse_punch_ack(&encoded), Some((12, 123456)));
    }

    #[test]
    fn test_parse_punch_ack_rejects_wrong_tag_or_length() {
        let mut wrong_tag = encode_punch_ack(12, 123456);
        wrong_tag[0] = 0xFF;
        assert_eq!(parse_punch_ack(&wrong_tag), None);
        assert_eq!(parse_punch_ack(&encode_punch_ack(12, 123456)[..6]), None);
        assert_eq!(parse_punch_ack(&[]), None);
    }

    #[test]
    fn test_parse_rejects_unrelated_text() {
        assert_eq!(Frame::parse("HB"), None);
        assert_eq!(Frame::parse("PUNCH 123 31:36070"), None);
        assert_eq!(Frame::parse(""), None);
    }

    #[test]
    fn test_parse_rejects_malformed_command() {
        assert_eq!(Frame::parse("CMD notanumber 1 hb_interval=30"), None);
        assert_eq!(Frame::parse("CMD 5 notanumber hb_interval=30"), None);
        assert_eq!(Frame::parse("CMD 5 1 unknown_setting=30"), None);
        assert_eq!(Frame::parse("CMD 5 1 hb_interval=notanumber"), None);
        assert_eq!(Frame::parse("CMD 5"), None);
        assert_eq!(Frame::parse("CMD 5 1"), None);
    }

    #[test]
    fn test_version_query_round_trips() {
        let frame = Frame::VersionQuery { target: 10 };
        let encoded = frame.encode();
        assert_eq!(encoded, "VQUERY 10");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_version_report_round_trips() {
        let frame = Frame::VersionReport { origin: 10, version: "0.1.0+a1b2c3d4.dirty".to_string() };
        let encoded = frame.encode();
        assert_eq!(encoded, "VERSION 10 0.1.0+a1b2c3d4.dirty");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_parse_rejects_malformed_version_query() {
        assert_eq!(Frame::parse("VQUERY notanumber"), None);
        assert_eq!(Frame::parse("VQUERY"), None);
    }

    #[test]
    fn test_parse_rejects_malformed_version_report() {
        assert_eq!(Frame::parse("VERSION notanumber 0.1.0"), None);
        assert_eq!(Frame::parse("VERSION 10"), None);
        assert_eq!(Frame::parse("VERSION 10 "), None);
        assert_eq!(Frame::parse("VERSION"), None);
    }

    #[test]
    fn test_config_ack_round_trips() {
        let frame = Frame::ConfigAck { target: 10 };
        let encoded = frame.encode();
        assert_eq!(encoded, "CACK 10");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_parse_rejects_malformed_config_ack() {
        assert_eq!(Frame::parse("CACK notanumber"), None);
        assert_eq!(Frame::parse("CACK"), None);
    }
}
