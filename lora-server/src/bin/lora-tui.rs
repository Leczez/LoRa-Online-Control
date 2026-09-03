use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "lora-tui", about = "Interactive terminal UI that attaches to a running lora-server daemon")]
struct Args {
    /// Unix socket of the lora-server daemon to attach to.
    #[arg(long, env = "LORA_SOCKET", default_value = "/run/lora-server/control.sock")]
    socket: String,

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
    lora_server::backend::attach(&args.socket, args.addr, args.dest)
}
