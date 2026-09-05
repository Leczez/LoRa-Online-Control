use clap::Parser;
use lora_server::Args;

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
    load_env_file("/etc/lora-server/env");

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    let args = Args::parse();

    log::info!(
        "starting on SPI (reset pin {}) addr {} dest {} freq {}MHz sf {} bw {}Hz heartbeat {}s",
        args.reset_pin, args.addr, args.dest, args.freq, args.sf, args.bw_hz, args.heartbeat_interval
    );
    lora_server::backend::run(args)
}
