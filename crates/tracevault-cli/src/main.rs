use std::io::IsTerminal;

use clap::Parser;
use tracevault_cli::cli::{deprecation_notice, run, Command};

#[derive(Parser)]
#[command(name = "tracevault", version, about = "AI code governance platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // stderr and a terminal only: hook output on stdout is parsed by agents,
    // and scripts capturing stderr must not see the notice.
    if std::io::stderr().is_terminal() {
        if let Some(notice) = deprecation_notice(&cli.command) {
            eprintln!("{notice}");
        }
    }
    run(cli.command).await;
}
