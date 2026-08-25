mod app;
mod backend;
mod sportident;
mod ui;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-cli", about = "Interactive LoRa terminal")]
pub struct Args {
    /// Serial port path (e.g. /dev/ttyS0 or /dev/ttyUSB0)
    #[arg(long, env = "LORA_PORT", required_unless_present = "attach")]
    pub port: Option<String>,

    /// Frequency in MHz (410-493 or 850-930)
    #[arg(long, env = "LORA_FREQ", default_value_t = 868)]
    pub freq: u32,

    /// Node address (0-65535)
    #[arg(long, env = "LORA_ADDR", default_value_t = 0)]
    pub addr: u16,

    /// Destination address for sent messages (0-65535)
    #[arg(long, env = "LORA_DEST", default_value_t = 1)]
    pub dest: u16,

    /// TX power in dBm (10, 13, 17, or 22)
    #[arg(long, env = "LORA_POWER", default_value_t = 22)]
    pub power: u8,

    /// Air speed in bps (1200, 2400, 4800, 9600, 19200, 38400, 62500)
    #[arg(long, env = "LORA_AIR_SPEED", default_value_t = 2400)]
    pub air_speed: u32,

    /// M0 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long, env = "LORA_M0_PIN")]
    pub m0_pin: Option<u8>,

    /// M1 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long, env = "LORA_M1_PIN")]
    pub m1_pin: Option<u8>,

    /// Heartbeat interval in seconds (0 to disable)
    #[arg(long, env = "LORA_HEARTBEAT_INTERVAL", default_value_t = 60)]
    pub heartbeat_interval: u64,

    /// Unix socket path used by the daemon
    #[arg(long, default_value = "/run/lora-cli/control.sock")]
    pub socket: String,

    /// Attach to a running daemon instead of starting one
    #[arg(long)]
    pub attach: bool,
}

fn load_env_file(path: &str) {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if std::env::var(key).is_err() {
                    unsafe { std::env::set_var(key, val) };
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    load_env_file("/etc/lora-cli/env");

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    let args = Args::parse();

    if args.attach {
        return backend::attach(&args.socket, args.addr, args.dest);
    }

    log::info!(
        "starting on port {} addr {} dest {} freq {}MHz air_speed {}bps heartbeat {}s",
        args.port.as_deref().unwrap_or(""),
        args.addr, args.dest, args.freq, args.air_speed, args.heartbeat_interval
    );
    backend::run(args)
}
