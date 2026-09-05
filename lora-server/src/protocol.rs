// lora-server/src/protocol.rs
//
// Wire format for the control-plane frames layered on top of the existing
// free-text "HB"/"PUNCH ..." payloads (see sportident.rs and
// docs/protocols/lora_online_control_protocol.md): a downlink `CMD` frame
// lets the base station change a limited set of settings on a specific
// node, and an uplink `ACK` frame confirms it landed. Scope is deliberately
// narrow — only settings that can't strand a node if misapplied (see the
// protocol doc's "Command Packets" section) get a `Setting` variant here.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Downlink: base station -> node at `target`, asking it to change `setting`.
    Command { target: u16, setting: Setting },
    /// Uplink: node at `origin` confirming it applied `setting`.
    Ack { origin: u16, setting: Setting },
    /// Confirms to `node` that its punch for `card_id` was received. A node
    /// holds its next punch (stop-and-wait, see the protocol doc) until this
    /// arrives or a retry timeout elapses, so only one punch is ever
    /// unacknowledged at a time per node — `card_id` alone is enough to
    /// disambiguate since there's never more than one outstanding.
    PunchAck { node: u16, card_id: u32 },
}

impl Frame {
    pub fn encode(&self) -> String {
        match self {
            Frame::Command { target, setting } => format!("CMD {} {}", target, setting.encode()),
            Frame::Ack { origin, setting } => format!("ACK {} {}", origin, setting.encode()),
            Frame::PunchAck { node, card_id } => format!("PACK {} {}", node, card_id),
        }
    }

    pub fn parse(s: &str) -> Option<Frame> {
        if let Some(rest) = s.strip_prefix("CMD ") {
            let mut parts = rest.splitn(2, ' ');
            let target: u16 = parts.next()?.parse().ok()?;
            let setting = Setting::parse(parts.next()?)?;
            return Some(Frame::Command { target, setting });
        }
        if let Some(rest) = s.strip_prefix("ACK ") {
            let mut parts = rest.splitn(2, ' ');
            let origin: u16 = parts.next()?.parse().ok()?;
            let setting = Setting::parse(parts.next()?)?;
            return Some(Frame::Ack { origin, setting });
        }
        if let Some(rest) = s.strip_prefix("PACK ") {
            let mut parts = rest.splitn(2, ' ');
            let node: u16 = parts.next()?.parse().ok()?;
            let card_id: u32 = parts.next()?.parse().ok()?;
            return Some(Frame::PunchAck { node, card_id });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_round_trips() {
        let frame = Frame::Command { target: 5, setting: Setting::HeartbeatIntervalSecs(30) };
        let encoded = frame.encode();
        assert_eq!(encoded, "CMD 5 hb_interval=30");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_ack_round_trips() {
        let frame = Frame::Ack { origin: 7, setting: Setting::HeartbeatIntervalSecs(45) };
        let encoded = frame.encode();
        assert_eq!(encoded, "ACK 7 hb_interval=45");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_punch_ack_round_trips() {
        let frame = Frame::PunchAck { node: 12, card_id: 123456 };
        let encoded = frame.encode();
        assert_eq!(encoded, "PACK 12 123456");
        assert_eq!(Frame::parse(&encoded), Some(frame));
    }

    #[test]
    fn test_parse_rejects_malformed_punch_ack() {
        assert_eq!(Frame::parse("PACK notanumber 123"), None);
        assert_eq!(Frame::parse("PACK 12 notanumber"), None);
        assert_eq!(Frame::parse("PACK 12"), None);
    }

    #[test]
    fn test_parse_rejects_unrelated_text() {
        assert_eq!(Frame::parse("HB"), None);
        assert_eq!(Frame::parse("PUNCH 123 31:36070"), None);
        assert_eq!(Frame::parse(""), None);
    }

    #[test]
    fn test_parse_rejects_malformed_command() {
        assert_eq!(Frame::parse("CMD notanumber hb_interval=30"), None);
        assert_eq!(Frame::parse("CMD 5 unknown_setting=30"), None);
        assert_eq!(Frame::parse("CMD 5 hb_interval=notanumber"), None);
        assert_eq!(Frame::parse("CMD 5"), None);
    }
}
