use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-tui", about = "Interactive terminal UI that attaches to a running lora-server daemon")]
struct Args {
    /// Base URL of the lora-server daemon's web API to attach to.
    #[arg(long, env = "LORA_SERVER_URL", default_value = "http://127.0.0.1:8082")]
    server_url: String,

    /// Node address to display (0-65535)
    #[arg(long, env = "LORA_ADDR", default_value_t = 0)]
    addr: u16,

    /// Destination address for sent messages (0-65535)
    #[arg(long, env = "LORA_DEST", default_value_t = 1)]
    dest: u16,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .format_timestamp(None)
        .init();

    let args = Args::parse();
    lora_server::backend::attach(&args.server_url, args.addr, args.dest)
}
