use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-server", about = "LoRa daemon: radio I/O, SportIdent reading, punch buffering")]
pub struct Args {
    /// RESET GPIO pin for the SPI radio (BCM, Raspberry Pi only)
    #[arg(long, env = "LORA_RESET_PIN", default_value_t = 25)]
    pub reset_pin: u8,

    /// LoRa spreading factor for the SPI radio (7-12)
    #[arg(long, env = "LORA_SF", default_value_t = 7)]
    pub sf: u8,

    /// LoRa bandwidth in Hz for the SPI radio (e.g. 125000)
    #[arg(long, env = "LORA_BW_HZ", default_value_t = 125_000)]
    pub bw_hz: u32,

    /// LoRa coding rate denominator for the SPI radio (5-8, meaning 4/5..4/8)
    #[arg(long, env = "LORA_CR", default_value_t = 5)]
    pub cr: u8,

    /// LoRa sync word for the SPI radio (decimal; default 18 = 0x12)
    #[arg(long, env = "LORA_SYNC_WORD", default_value_t = 18)]
    pub sync_word: u8,

    /// Frequency in MHz (410-493 or 850-930)
    #[arg(long, env = "LORA_FREQ", default_value_t = 868)]
    pub freq: u32,

    /// Node address (0-65535)
    #[arg(long, env = "LORA_ADDR", default_value_t = 0)]
    pub addr: u16,

    /// Destination address for sent messages (0-65535). For a node acting
    /// as a relay (--relay), this also doubles as the next hop for traffic
    /// it forwards on behalf of other nodes.
    #[arg(long, env = "LORA_DEST", default_value_t = 1)]
    pub dest: u16,

    /// Act as a relay: forward punches/acks/commands this node receives
    /// that aren't its own, on toward --dest, rather than treating itself
    /// as the final consumer. A node can be a relay and still have its own
    /// local SI reader — the two roles are independent (see the "Relay
    /// Nodes" section of docs/protocols/lora_online_control_protocol.md).
    #[arg(long, env = "LORA_RELAY", default_value_t = false)]
    pub relay: bool,

    /// TX power in dBm (10, 13, 17, or 22)
    #[arg(long, env = "LORA_POWER", default_value_t = 22)]
    pub power: u8,

    /// Heartbeat interval in seconds (0 to disable)
    #[arg(long, env = "LORA_HEARTBEAT_INTERVAL", default_value_t = 60)]
    pub heartbeat_interval: u64,

    /// Unix socket path this daemon binds, for lora-tui (or other clients) to attach to.
    #[arg(long, default_value = "/run/lora-server/control.sock")]
    pub socket: String,

    /// Path to the persistent punch buffer (SQLite). Every punch, local or
    /// remote, is recorded here before anything else happens to it.
    #[arg(long, env = "LORA_PUNCH_DB", default_value = "/var/lib/lora-server/punches.db")]
    pub punch_db: String,

    /// URL of the remote roc-server's ingestion endpoint (e.g.
    /// http://100.x.y.z:8080/punches). If unset, punches are still buffered
    /// locally but never pushed anywhere.
    #[arg(long, env = "LORA_PUSH_TO")]
    pub push_to: Option<String>,

    /// How often the background pusher checks the buffer for unsent punches.
    #[arg(long, env = "LORA_PUSH_INTERVAL_SECS", default_value_t = 10)]
    pub push_interval_secs: u64,
}
