//! secure-send-cli: CLI companion to secure-send-web for peer-to-peer file transfer.
//!
//! Nostr PIN mode matches secure-send-web's Auto Exchange flow. Manual
//! copy/paste mode exchanges SS03 offer/answer codes. QR support is intentionally
//! not part of this CLI.
//! Build with: cargo build --release

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use secure_send_cli::util::is_interrupted;
use secure_send_cli::{ui, webrtc};

#[derive(Parser)]
#[command(name = "secure-send-cli")]
#[command(about = "Secure peer-to-peer file transfer, compatible with secure-send-web")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Use verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a file. Defaults to secure-send-web Nostr PIN mode.
    Send {
        /// Path to the file to send
        path: PathBuf,

        /// Use manual SS03 copy/paste signaling instead of Nostr PIN mode
        #[arg(long)]
        manual: bool,
    },

    /// Receive a file. Defaults to secure-send-web Nostr PIN mode.
    Receive {
        /// PIN for Nostr mode, or sender offer code with --manual
        code: Option<String>,

        /// Output directory (defaults to the current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Use manual SS03 copy/paste signaling instead of Nostr PIN mode
        #[arg(long)]
        manual: bool,
    },
}

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install Rustls crypto provider");

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime")
        .block_on(async_main());

    if let Err(e) = result {
        if is_interrupted(&e) {
            // 128 + SIGINT(2) = 130, the conventional Unix exit code.
            std::process::exit(130);
        }
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();

    // Without --verbose, keep the transfer output clean: suppress info/debug/trace
    // log noise from this crate and its dependencies, leaving only warnings and
    // errors. --verbose opts into full debug logging. RUST_LOG still overrides both.
    let log_level = if cli.verbose { "debug" } else { "warn" };
    let filter = format!("{log_level},webrtc_ice=error");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&filter)).init();

    match cli.command {
        Commands::Send { path, manual } => {
            if path.is_dir() {
                anyhow::bail!(
                    "Path is a directory: {} (only single files can be sent)",
                    path.display()
                );
            }
            if manual {
                webrtc::send_file_manual(&path).await?;
            } else {
                webrtc::send_file_nostr(&path).await?;
            }
        }

        Commands::Receive {
            code,
            output,
            manual,
        } => {
            if manual {
                let offer_code = match code {
                    Some(c) => c,
                    None => ui::prompt_multiline("Paste the sender's offer code:")?,
                };
                webrtc::receive_file_manual(offer_code.trim(), output).await?;
            } else {
                let pin = match code {
                    Some(c) => c,
                    None => ui::prompt_line("Enter secure-send PIN: ")?,
                };
                webrtc::receive_file_nostr(pin.trim(), output).await?;
            }
        }
    }

    Ok(())
}
