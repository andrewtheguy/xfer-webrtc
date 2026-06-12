//! beam-rs-webrtc: WebRTC transport for peer-to-peer file transfer
//!
//! This crate provides file transfer using WebRTC data channels with
//! Nostr relays for signaling. It supports both online (Nostr) and
//! offline (copy/paste) signaling modes.
//!
//! Build with: cargo build --release

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use beam_rs_webrtc::core::transfer::is_interrupted;
use beam_rs_webrtc::signaling::offline::{self, ReceiveInput};
use beam_rs_webrtc::webrtc;

#[derive(Parser)]
#[command(name = "beam-rs-webrtc")]
#[command(about = "Secure file transfer using WebRTC for peer-to-peer connectivity")]
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
    /// Send a file using WebRTC transport
    Send {
        /// Path to file or folder to send
        path: PathBuf,

        /// Use manual signaling (copy/paste SDP offers) instead of Nostr
        #[arg(long)]
        manual: bool,

        /// Use default Nostr relays instead of auto-discovery
        #[arg(long)]
        default_relays: bool,

        /// Custom Nostr relay URLs (can be specified multiple times)
        #[arg(long, value_name = "URL")]
        relay: Vec<String>,
    },

    /// Receive a file using WebRTC transport
    ///
    /// Automatically detects whether the input is a Nostr beam code or a
    /// manual copy/paste offer.
    Receive {
        /// Beam code from sender (will prompt if not provided)
        code: Option<String>,

        /// Output directory (defaults to current directory)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Disable resumable transfers (don't save partial downloads)
        #[arg(long)]
        no_resume: bool,
    },
}

fn main() {
    // Run the async main and handle errors
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime")
        .block_on(async_main());

    if let Err(e) = result {
        // Check if this was an interrupt (Ctrl+C)
        if is_interrupted(&e) {
            // Exit with 128 + SIGINT (2) = 130, standard Unix convention
            std::process::exit(130);
        }
        // Print error and exit with failure code
        eprintln!("Error: {:?}", e);
        std::process::exit(1);
    }
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging with filters for noisy internal modules
    let log_level = if cli.verbose { "debug" } else { "info" };
    let filter = format!("{},webrtc_ice=error,nostr_relay_pool=warn", log_level);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&filter)).init();

    match cli.command {
        Commands::Send {
            path,
            manual,
            default_relays,
            relay,
        } => {
            if manual {
                if path.is_dir() {
                    webrtc::offline_sender::send_folder_offline(&path).await?;
                } else {
                    webrtc::offline_sender::send_file_offline(&path).await?;
                }
            } else {
                let custom_relays = if relay.is_empty() { None } else { Some(relay) };

                if path.is_dir() {
                    webrtc::send_folder_webrtc(&path, custom_relays, default_relays).await?;
                } else {
                    webrtc::send_file_webrtc(&path, custom_relays, default_relays).await?;
                }
            }
        }

        Commands::Receive {
            code,
            output,
            no_resume,
        } => {
            // A beam code given on the command line is always an automatic
            // transfer; otherwise read from stdin and auto-detect the mode.
            let input = match code {
                Some(c) => ReceiveInput::Code(c.trim().to_string()),
                None => offline::read_code_or_offer()?,
            };

            match input {
                ReceiveInput::Code(code) => {
                    if code.is_empty() {
                        anyhow::bail!("Beam code is required");
                    }
                    webrtc::receive_webrtc(&code, output, no_resume).await?;
                }
                ReceiveInput::Manual(offer) => {
                    webrtc::receive_file_offline(*offer, output, no_resume).await?;
                }
            }
        }
    }

    Ok(())
}
