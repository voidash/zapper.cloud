use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use console::style;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use zap_core::{ReceiveProgress, SendProgress, Ticket, ZapNode};

/// Default relay server for short codes
const DEFAULT_RELAY: &str = "https://zapper.cloud";

#[derive(Parser)]
#[command(name = "zap")]
#[command(about = "Fast, secure file transfers", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Send a file or folder
    Send {
        /// Path to the file or folder to send (interactive if not provided)
        path: Option<PathBuf>,

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
        output: Option<PathBuf>,

        /// Custom relay server URL
        #[arg(long, default_value = DEFAULT_RELAY)]
        relay: String,
    },
}

#[derive(Serialize)]
struct RegisterRequest {
    ticket: String,
    file_name: Option<String>,
}

#[derive(Deserialize)]
struct RegisterResponse {
    code: String,
    words: String,
}

#[derive(Deserialize)]
struct LookupResponse {
    ticket: String,
}

pub async fn run_send(path: Option<PathBuf>, no_relay: bool, relay: String) -> Result<()> {
    // Interactive file selection if no path provided
    let path = match path {
        Some(p) => p,
        None => select_file_interactive()?,
    };

    // Validate path exists
    if !path.exists() {
        anyhow::bail!("Path does not exist: {}", path.display());
    }

    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    println!(
        "\n{} Preparing to send: {}",
        style("⚡").cyan(),
        style(&file_name).green()
    );

    let node = ZapNode::new().await?;
    let (ticket, mut progress_rx) = node.send(&path).await?;

    // Register with relay to get short code
    let code_info = if no_relay {
        None
    } else {
        match register_ticket(&relay, &ticket.to_string(), Some(&file_name)).await {
            Ok(info) => Some(info),
            Err(e) => {
                eprintln!(
                    "{} Could not register with relay: {}",
                    style("⚠").yellow(),
                    e
                );
                None
            }
        }
    };

    println!();
    if let Some(ref info) = code_info {
        println!(
            "{} Share this code with the receiver:\n",
            style("⚡").cyan()
        );
        println!("  Code:  {}", style(&info.code).green().bold());
        println!("  Words: {}", style(&info.words).cyan().bold());
        println!();
        println!(
            "  {}",
            style("Receiver runs: zap receive <code>").dim()
        );
    } else {
        println!(
            "{} Share this ticket with the receiver:\n",
            style("⚡").cyan()
        );
        println!("  {}", style(ticket.to_string()).green());
    }

    println!();
    println!("{}", style("Waiting for receiver to connect...").dim());

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    while let Some(progress) = progress_rx.recv().await {
        match progress {
            SendProgress::Waiting => {}
            SendProgress::Connected => {
                println!("{}", style("Receiver connected!").green());
            }
            SendProgress::Sending {
                bytes_sent,
                total_bytes,
            } => {
                pb.set_length(total_bytes);
                pb.set_position(bytes_sent);
            }
            SendProgress::Complete => {
                pb.finish_with_message("done");
                println!("\n{} Transfer complete!", style("✓").green().bold());
                break;
            }
            SendProgress::Error(e) => {
                pb.abandon();
                anyhow::bail!("Transfer failed: {}", e);
            }
        }
    }

    node.shutdown().await?;
    Ok(())
}

pub async fn run_receive(
    code: Option<String>,
    output: Option<PathBuf>,
    relay: String,
) -> Result<()> {
    // Interactive code input if not provided
    let code = match code {
        Some(c) => c,
        None => Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter code or ticket")
            .interact_text()?,
    };

    let code = code.trim();

    // Determine if it's a short code/words or full ticket
    let ticket_str = if is_short_code(code) {
        println!(
            "{} Looking up code: {}",
            style("⚡").cyan(),
            style(code).green()
        );
        lookup_ticket(&relay, code).await?
    } else {
        code.to_string()
    };

    let ticket = Ticket::deserialize(&ticket_str)?;
    let node = ZapNode::new().await?;

    let mut progress_rx = node.receive(ticket, output.as_deref()).await?;

    println!("\n{} Connecting to sender...", style("⚡").cyan());

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("=>-"),
    );

    while let Some(progress) = progress_rx.recv().await {
        match progress {
            ReceiveProgress::Connecting => {}
            ReceiveProgress::Connected => {
                println!("{}", style("Connected!").green());
            }
            ReceiveProgress::Offer { name, size } => {
                println!(
                    "Receiving {} ({})",
                    style(&name).cyan(),
                    format_bytes(size)
                );
            }
            ReceiveProgress::Receiving {
                bytes_received,
                total_bytes,
            } => {
                pb.set_length(total_bytes);
                pb.set_position(bytes_received);
            }
            ReceiveProgress::Complete { path } => {
                pb.finish_with_message("done");
                println!(
                    "\n{} Saved to {}",
                    style("✓").green().bold(),
                    style(path.display()).cyan()
                );
                break;
            }
            ReceiveProgress::Error(e) => {
                pb.abandon();
                anyhow::bail!("Transfer failed: {}", e);
            }
        }
    }

    node.shutdown().await?;
    Ok(())
}

/// Interactive file/folder selection
fn select_file_interactive() -> Result<PathBuf> {
    println!(
        "\n{} What would you like to send?",
        style("⚡").cyan()
    );

    let options = vec!["Select a file", "Enter path manually"];
    let selection = Select::with_theme(&ColorfulTheme::default())
        .items(&options)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            // List current directory files
            let cwd = std::env::current_dir()?;
            let mut entries: Vec<_> = std::fs::read_dir(&cwd)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            entries.sort();

            if entries.is_empty() {
                anyhow::bail!("No files in current directory");
            }

            let display_names: Vec<String> = entries
                .iter()
                .map(|p| {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if p.is_dir() {
                        format!("{}/", name)
                    } else {
                        name
                    }
                })
                .collect();

            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Select file or folder")
                .items(&display_names)
                .default(0)
                .interact()?;

            Ok(entries[selection].clone())
        }
        _ => {
            let input: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Enter path")
                .interact_text()?;

            let path = PathBuf::from(shellexpand::tilde(&input).to_string());
            Ok(path)
        }
    }
}

/// Check if the input looks like a short code or word-based code
fn is_short_code(input: &str) -> bool {
    // Word-based code (contains hyphens, like "alpha-bravo-charlie")
    if input.contains('-') && input.split('-').all(|w| w.chars().all(|c| c.is_alphabetic())) {
        return true;
    }

    // Short alphanumeric code (6 chars or less)
    if input.len() <= 8 && input.chars().all(|c| c.is_alphanumeric()) {
        return true;
    }

    false
}

/// Register a ticket with the relay server
async fn register_ticket(
    relay: &str,
    ticket: &str,
    file_name: Option<&str>,
) -> Result<RegisterResponse> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/register", relay))
        .json(&RegisterRequest {
            ticket: ticket.to_string(),
            file_name: file_name.map(String::from),
        })
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Relay returned error: {}", resp.status());
    }

    Ok(resp.json().await?)
}

/// Look up a ticket from the relay server
async fn lookup_ticket(relay: &str, code: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/lookup/{}", relay, code))
        .send()
        .await?;

    if !resp.status().is_success() {
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!("Code not found or expired. Make sure the sender is still running.");
        }
        anyhow::bail!("Relay returned error: {}", resp.status());
    }

    let data: LookupResponse = resp.json().await?;
    Ok(data.ticket)
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
