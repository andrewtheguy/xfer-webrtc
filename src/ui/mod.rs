//! Terminal output and interactive prompts for the CLI.
//!
//! Every transfer flow reports through the free functions here. By default
//! they print plain text: status/progress to stderr, and the base64 signaling
//! codes the user must copy to stdout so they can be piped or redirected
//! cleanly. When the TUI wizard runs a Nostr transfer it installs an event
//! sink first ([`install_tui_sink`]); the same functions then emit
//! [`UiEvent`]s for the TUI to render instead of printing. The sink is
//! installed at most once per process — the wizard performs a single transfer
//! and exits.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::util::{calc_percent, format_bytes};

/// Direction of a transfer, used to label progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

/// User's choice when a destination file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileExistsChoice {
    Overwrite,
    Rename,
    Cancel,
}

/// What a transfer flow reports while running, for the TUI to render.
#[derive(Debug)]
pub enum UiEvent {
    Status(String),
    /// Completion of the most recent [`UiEvent::Status`] step ("Doing X..." →
    /// "Did X (elapsed)"); the TUI replaces that line instead of appending.
    StatusDone(String),
    Progress { dir: Direction, bytes: u64, total: u64 },
    ProgressEnd,
    ShowPin {
        file_name: String,
        size: u64,
        pin: String,
        fingerprint: String,
    },
    /// The PIN is no longer valid (a receiver claimed the transfer); stop
    /// displaying it.
    HidePin,
    Incoming { file_name: String, size: u64 },
    FileExists { path: PathBuf, reply: oneshot::Sender<FileExistsChoice> },
}

static TUI_SINK: OnceLock<mpsc::UnboundedSender<UiEvent>> = OnceLock::new();

/// On-demand PIN refresh requests (TUI `r` key → Nostr sender). A
/// [`Notify`] with `notify_one` semantics: a request made moments before the
/// sender awaits is stored as a permit, not lost.
static PIN_REFRESH: OnceLock<Arc<Notify>> = OnceLock::new();

/// The shared PIN-refresh signal. The Nostr sender awaits it while waiting
/// for a receiver; [`request_pin_refresh`] fires it.
pub fn pin_refresh_signal() -> Arc<Notify> {
    PIN_REFRESH.get_or_init(|| Arc::new(Notify::new())).clone()
}

/// Ask the running Nostr sender to mint and publish a fresh PIN immediately,
/// invalidating every previously shown PIN. No-op unless a transfer is
/// waiting for a receiver.
pub fn request_pin_refresh() {
    pin_refresh_signal().notify_one();
}

/// Route all subsequent UI output to the TUI as [`UiEvent`]s. Call once,
/// before spawning the transfer task.
pub fn install_tui_sink(tx: mpsc::UnboundedSender<UiEvent>) {
    // The wizard installs the sink once per process. Guard against a repeat
    // call anyway: keep the first sender rather than crashing or replacing it.
    if TUI_SINK.set(tx).is_err() {
        debug_assert!(false, "TUI sink installed more than once");
    }
}

fn sink() -> Option<&'static mpsc::UnboundedSender<UiEvent>> {
    TUI_SINK.get()
}

/// Informational status line (stderr).
pub fn status(line: &str) {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::Status(line.to_string()));
    } else {
        eprintln!("{line}");
    }
}

/// Informational status line with elapsed time, completing the step announced
/// by the preceding [`status`] call.
pub fn status_timed(line: &str, elapsed: Duration) {
    let full = format!("{line} ({})", format_elapsed(elapsed));
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::StatusDone(full));
    } else {
        eprintln!("{full}");
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// A base64 signaling code the user must copy (stdout, framed for readability).
pub fn show_code(title: &str, code: &str) {
    println!("\n----- {title} -----");
    println!("{code}");
    println!("----- end -----\n");
    let _ = std::io::stdout().flush();
}

/// Update the single-line live progress indicator (stderr).
pub fn progress(dir: Direction, bytes: u64, total: u64) {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::Progress { dir, bytes, total });
        return;
    }
    let verb = match dir {
        Direction::Send => "Sending",
        Direction::Receive => "Receiving",
    };
    eprint!(
        "\r   {verb}: {}% ({}/{})",
        calc_percent(bytes, total) as u32,
        format_bytes(bytes),
        format_bytes(total),
    );
    let _ = std::io::stderr().flush();
}

/// Terminate the live progress line with a newline.
pub fn progress_end() {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::ProgressEnd);
    } else {
        eprintln!();
    }
}

/// Present the sender's PIN (stdout in plain mode, panel in the TUI) along
/// with what is being sent and the fingerprint for visual verification.
pub fn show_pin(file_name: &str, file_size: u64, pin: &str, fingerprint: &str) {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::ShowPin {
            file_name: file_name.to_string(),
            size: file_size,
            pin: pin.to_string(),
            fingerprint: fingerprint.to_string(),
        });
        return;
    }
    eprintln!(
        "Ready to send \"{file_name}\" ({}). Enter this PIN in secure-send-web:",
        format_bytes(file_size)
    );
    println!("{pin}");
    eprintln!("PIN fingerprint: {fingerprint} (should match the receiver's)");
}

/// Stop displaying the PIN: a receiver claimed the transfer, so every shown
/// PIN is now invalid. Plain mode prints nothing — the sender's status line
/// already reports the claim.
pub fn hide_pin() {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::HidePin);
    }
}

/// Announce the incoming file a receiver is about to accept.
pub fn incoming(file_name: &str, size: u64, mime_type: Option<&str>) {
    if let Some(tx) = sink() {
        let _ = tx.send(UiEvent::Incoming {
            file_name: file_name.to_string(),
            size,
        });
        return;
    }
    match mime_type {
        Some(mime) => eprintln!(
            "Incoming file: \"{file_name}\" ({}, {mime})",
            format_bytes(size)
        ),
        None => eprintln!("Incoming file: \"{file_name}\" ({})", format_bytes(size)),
    }
}

/// Ask how to handle an existing destination file.
pub async fn prompt_file_exists(path: &Path) -> Result<FileExistsChoice> {
    if let Some(tx) = sink() {
        let (reply, rx) = oneshot::channel();
        tx.send(UiEvent::FileExists {
            path: path.to_path_buf(),
            reply,
        })
        .map_err(|_| anyhow!("TUI closed"))?;
        return rx.await.map_err(|_| anyhow!("TUI closed"));
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || prompt_file_exists_blocking(&path)).await?
}

fn prompt_file_exists_blocking(path: &Path) -> Result<FileExistsChoice> {
    print!(
        "Warning: file exists: {}\n[o]verwrite / [r]ename / [c]ancel: ",
        path.display()
    );
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    match input.trim().to_lowercase().as_str() {
        "o" | "overwrite" => Ok(FileExistsChoice::Overwrite),
        "r" | "rename" => Ok(FileExistsChoice::Rename),
        _ => Ok(FileExistsChoice::Cancel),
    }
}

/// Read a pasted code, submitted with a single Enter.
///
/// Base64 SS03 codes are single-line, but a paste may carry hard line breaks
/// (e.g. copied from wrapped text). A multi-line paste lands in the input
/// buffer all at once, so after the first line we briefly drain whatever else
/// is already there and join it — one Enter still submits.
pub async fn prompt_code(prompt: &str) -> Result<String> {
    use tokio::io::AsyncBufReadExt;

    eprintln!("{prompt}");
    eprintln!("(paste the code and press Enter)");
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    let mut collected = loop {
        let line = lines.next_line().await?.ok_or_else(|| anyhow!("no code entered"))?;
        let line = line.trim();
        if !line.is_empty() {
            break line.to_string();
        }
        // ignore leading blank lines
    };

    const PASTE_DRAIN_WINDOW: Duration = Duration::from_millis(80);
    while let Ok(Ok(Some(line))) =
        tokio::time::timeout(PASTE_DRAIN_WINDOW, lines.next_line()).await
    {
        collected.push_str(line.trim());
    }

    Ok(collected)
}
