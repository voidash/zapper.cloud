use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

const DEFAULT_RELAY: &str = "https://zapper.cloud";

#[derive(Parser)]
#[command(name = "zap")]
#[command(version, about = "Fast, secure file transfers", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a file or folder
    Send {
        /// Path to the file or folder to send (interactive if not provided)
        path: Option<std::path::PathBuf>,

        /// Don't use relay for short codes (share full ticket instead)
        #[arg(long)]
        no_relay: bool,

        /// Custom relay server URL
        #[arg(long, default_value = DEFAULT_RELAY)]
        relay: String,
    },

    /// Receive a file
    Receive {
        /// The code or ticket from the sender (interactive if not provided)
        code: Option<String>,

        /// Output directory (defaults to current directory)
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,

        /// Custom relay server URL
        #[arg(long, default_value = DEFAULT_RELAY)]
        relay: String,
    },

    /// Start the web server
    Serve {
        /// Address to bind to
        #[arg(short, long, default_value = "0.0.0.0:8080")]
        addr: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        Commands::Send {
            path,
            no_relay,
            relay,
        } => {
            zap_cli::run_send(path, no_relay, relay).await?;
        }
        Commands::Receive {
            code,
            output,
            relay,
        } => {
            zap_cli::run_receive(code, output, relay).await?;
        }
        Commands::Serve { addr } => {
            zap_web::run_server(addr).await?;
        }
    }

    Ok(())
}
