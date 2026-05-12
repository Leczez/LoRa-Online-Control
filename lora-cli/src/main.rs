mod app;
mod backend;
mod ui;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-cli", about = "Interactive LoRa terminal")]
pub struct Args {
    /// Serial port path (e.g. /dev/ttyS0 or /dev/ttyUSB0)
    #[arg(long)]
    pub port: String,

    /// Frequency in MHz (410-493 or 850-930)
    #[arg(long, default_value_t = 868)]
    pub freq: u32,

    /// Node address (0-65535)
    #[arg(long, default_value_t = 0)]
    pub addr: u16,

    /// Destination address for sent messages (0-65535)
    #[arg(long, default_value_t = 1)]
    pub dest: u16,

    /// TX power in dBm (10, 13, 17, or 22)
    #[arg(long, default_value_t = 22)]
    pub power: u8,

    /// Air speed in bps (1200, 2400, 4800, 9600, 19200, 38400, 62500)
    #[arg(long, default_value_t = 2400)]
    pub air_speed: u32,

    /// M0 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long)]
    pub m0_pin: Option<u8>,

    /// M1 GPIO pin number (BCM, Raspberry Pi only)
    #[arg(long)]
    pub m1_pin: Option<u8>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    backend::run(args)
}
