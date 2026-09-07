use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-server", about = "LoRa daemon: radio I/O, SportIdent reading, punch buffering")]
pub struct Args {
    /// RESET GPIO pin for the SPI radio (BCM, Raspberry Pi only)
    #[arg(long, env = "LORA_RESET_PIN", default_value_t = 25)]
    pub reset_pin: u8,

    /// Optional DIO0 GPIO pin (BCM, Raspberry Pi only). When set, TX/CAD
    /// completion is detected by polling this pin directly instead of
    /// IRQ_FLAGS over SPI — cheaper per check, since it's a plain GPIO read
    /// rather than an SPI transaction. Requires physically wiring the
    /// module's DIO0 pin to this GPIO; omit to keep polling over SPI as
    /// before (no extra wiring needed).
    #[arg(long, env = "LORA_DIO0_PIN")]
    pub dio0_pin: Option<u8>,

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

    /// Shared deployment identifier, prepended to every outgoing frame and
    /// checked on every received one — a plain-text guard against accidental
    /// cross-talk with another event running this same firmware nearby, not
    /// a security mechanism. Change this per event/deployment; every node
    /// and base station in one deployment must use the same value.
    #[arg(long, env = "LORA_NETWORK_ID", default_value = "LOC")]
    pub network_id: String,

    /// Address:port this daemon's own GET /health check server binds, so
    /// roc-server (or an operator) can confirm it's alive over the network —
    /// separate from the LoRa radio link's own liveness signal.
    #[arg(long, env = "LORA_HEALTH_LISTEN", default_value = "0.0.0.0:8081")]
    pub health_listen: String,

    /// roc-server's health-check URL (e.g. http://100.x.y.z:8080/health).
    /// If unset, this daemon doesn't check roc-server's reachability at all
    /// (it still serves its own /health regardless).
    #[arg(long, env = "LORA_ROC_HEALTH_URL")]
    pub roc_health_url: Option<String>,

    /// How often to check roc-server's health endpoint, in seconds.
    #[arg(long, env = "LORA_HEALTH_CHECK_INTERVAL_SECS", default_value_t = 30)]
    pub health_check_interval_secs: u64,

    /// Address:port for the browser-facing status dashboard (node health,
    /// packet log, send-a-test-punch form) — separate from --health-listen,
    /// which is deliberately minimal/machine-readable only.
    #[arg(long, env = "LORA_WEB_LISTEN", default_value = "0.0.0.0:8082")]
    pub web_listen: String,

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
