//! WebRTC common utilities for peer-to-peer file transfer
//!
//! This module contains:
//! - WebRTC peer connection management
//! - Data channel handlers

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::{SctpMaxMessageSize, SettingEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::candidate::CandidateType;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_gatherer_state::RTCIceGathererState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::stats::StatsReportType;

// ============================================================================
// Constants
// ============================================================================

/// Public STUN servers for NAT traversal. Matches secure-send-web's
/// `getWebRTCConfig()` so both peers probe the same reflexive candidates.
const STUN_SERVERS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
];

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert a CandidateType to a human-readable string
fn candidate_type_to_str(ct: CandidateType) -> &'static str {
    match ct {
        CandidateType::Host => "Host",
        CandidateType::ServerReflexive => "ServerReflexive",
        CandidateType::PeerReflexive => "PeerReflexive",
        CandidateType::Relay => "Relay",
        CandidateType::Unspecified => "Unspecified",
    }
}

// ============================================================================
// WebRTC Peer Connection
// ============================================================================

/// WebRTC peer connection wrapper
pub struct WebRtcPeer {
    peer_connection: Arc<RTCPeerConnection>,
    ice_candidate_rx: Option<mpsc::Receiver<RTCIceCandidate>>,
    data_channel_rx: Option<mpsc::Receiver<Arc<RTCDataChannel>>>,
    ice_gathering_rx: Option<watch::Receiver<RTCIceGathererState>>,
}

impl WebRtcPeer {
    /// Create a new WebRTC peer connection with STUN server for NAT traversal
    pub async fn new() -> Result<Self> {
        let ice_servers = vec![RTCIceServer {
            // Multiple public STUN endpoints improve NAT traversal resilience.
            urls: STUN_SERVERS.iter().map(|url| (*url).to_owned()).collect(),
            ..Default::default()
        }];
        Self::new_with_config(ice_servers).await
    }

    /// Internal helper to create peer connection with given ICE servers
    async fn new_with_config(ice_servers: Vec<RTCIceServer>) -> Result<Self> {
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let mut media_engine = MediaEngine::default();
        media_engine
            .register_default_codecs()
            .context("Failed to register default codecs")?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)
            .context("Failed to register interceptors")?;

        // Allow sending data-channel messages larger than the 64 KiB default so
        // a full 128 KiB content chunk fits in one message (secure-send-web's
        // wire format). The peer's advertised max-message-size still bounds us.
        let mut setting_engine = SettingEngine::default();
        setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);
        // Detach data channels so we own the read loop: the facade's built-in
        // reader caps messages at 65535 bytes, too small for a 128 KiB chunk.
        setting_engine.detach_data_channels();

        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        let peer_connection = Arc::new(
            api.new_peer_connection(config)
                .await
                .context("Failed to create peer connection")?,
        );

        let (ice_candidate_tx, ice_candidate_rx) = mpsc::channel(50);
        let (data_channel_tx, data_channel_rx) = mpsc::channel(1);
        let (ice_gathering_tx, ice_gathering_rx) = watch::channel(RTCIceGathererState::New);

        // Set up ICE candidate handler
        let ice_tx = ice_candidate_tx.clone();
        peer_connection.on_ice_candidate(Box::new(move |candidate| {
            let ice_tx = ice_tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate
                    && ice_tx.send(candidate).await.is_err()
                {
                    // Expected during shutdown when receiver is dropped
                    log::trace!("ICE candidate channel closed");
                }
            })
        }));

        // Set up ICE gathering state handler for non-trickle signaling.
        peer_connection.on_ice_gathering_state_change(Box::new(move |state| {
            if ice_gathering_tx.send(state).is_err() {
                // Expected during shutdown when receiver is dropped
                log::trace!("ICE gathering state channel closed");
            }
            Box::pin(async {})
        }));

        // Set up connection state handler
        peer_connection.on_peer_connection_state_change(Box::new(move |state| {
            Box::pin(async move {
                match state {
                    RTCPeerConnectionState::Connected => {
                        crate::ui::status("WebRTC connection established!");
                    }
                    RTCPeerConnectionState::Disconnected => {
                        crate::ui::status("WebRTC connection disconnected");
                    }
                    RTCPeerConnectionState::Failed => {
                        log::error!("WebRTC connection failed");
                    }
                    RTCPeerConnectionState::Closed => {
                        crate::ui::status("WebRTC connection closed");
                    }
                    _ => {}
                }
            })
        }));

        // Set up data channel handler (for incoming data channels)
        let dc_tx = data_channel_tx.clone();
        peer_connection.on_data_channel(Box::new(move |dc| {
            let dc_tx = dc_tx.clone();
            let label = dc.label().to_string();
            Box::pin(async move {
                if dc_tx.send(dc).await.is_err() {
                    // Expected during shutdown when receiver is dropped
                    log::trace!("Data channel '{}' receiver closed", label);
                }
            })
        }));

        Ok(Self {
            peer_connection,
            ice_candidate_rx: Some(ice_candidate_rx),
            data_channel_rx: Some(data_channel_rx),
            ice_gathering_rx: Some(ice_gathering_rx),
        })
    }

    /// Take ownership of the ICE candidate receiver
    #[allow(dead_code)]
    pub fn take_ice_candidate_rx(&mut self) -> Option<mpsc::Receiver<RTCIceCandidate>> {
        self.ice_candidate_rx.take()
    }

    /// Take ownership of the data channel receiver
    pub fn take_data_channel_rx(&mut self) -> Option<mpsc::Receiver<Arc<RTCDataChannel>>> {
        self.data_channel_rx.take()
    }

    /// Take ownership of the ICE gathering state receiver
    #[allow(dead_code)]
    pub fn take_ice_gathering_rx(&mut self) -> Option<watch::Receiver<RTCIceGathererState>> {
        self.ice_gathering_rx.take()
    }

    /// Wait for ICE gathering to complete and collect all candidates.
    /// This is used for "vanilla ICE" (non-trickle) signaling where we need
    /// all candidates before generating the offer/answer JSON.
    pub async fn gather_ice_candidates(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<RTCIceCandidate>> {
        let mut ice_rx = self
            .ice_candidate_rx
            .take()
            .context("ICE candidate receiver already taken")?;
        let mut gathering_rx = self
            .ice_gathering_rx
            .take()
            .context("ICE gathering receiver already taken")?;

        let mut candidates = Vec::new();

        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                // Timeout reached, return what we have
                eprintln!("ICE gathering timeout, collected {} candidates", candidates.len());
            }
            _ = async {
                loop {
                    tokio::select! {
                        candidate = ice_rx.recv() => {
                            match candidate {
                                Some(candidate) => {
                                    candidates.push(candidate);
                                }
                                None => {
                                    // Channel closed (sender dropped), drain any remaining and exit
                                    while let Ok(candidate) = ice_rx.try_recv() {
                                        candidates.push(candidate);
                                    }
                                    break;
                                }
                            }
                        }
                        result = gathering_rx.changed() => {
                            if result.is_ok() {
                                let state = *gathering_rx.borrow();
                                if state == RTCIceGathererState::Complete {
                                    // Give a small delay to collect any remaining candidates
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                    // Drain any remaining candidates
                                    while let Ok(candidate) = ice_rx.try_recv() {
                                        candidates.push(candidate);
                                    }
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
            } => {}
        }

        Ok(candidates)
    }

    /// Create a data channel with the given label
    pub async fn create_data_channel(&self, label: &str) -> Result<Arc<RTCDataChannel>> {
        let dc = self
            .peer_connection
            .create_data_channel(label, None)
            .await
            .context("Failed to create data channel")?;
        eprintln!("Created data channel: {}", label);
        Ok(dc)
    }

    /// Create an SDP offer
    pub async fn create_offer(&self) -> Result<RTCSessionDescription> {
        self.peer_connection
            .create_offer(None)
            .await
            .context("Failed to create offer")
    }

    /// Create an SDP answer
    pub async fn create_answer(&self) -> Result<RTCSessionDescription> {
        self.peer_connection
            .create_answer(None)
            .await
            .context("Failed to create answer")
    }

    /// Set the local description
    pub async fn set_local_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        self.peer_connection
            .set_local_description(sdp)
            .await
            .context("Failed to set local description")
    }

    /// Set the remote description
    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        self.peer_connection
            .set_remote_description(sdp)
            .await
            .context("Failed to set remote description")
    }

    /// Add an ICE candidate
    pub async fn add_ice_candidate(
        &self,
        candidate: webrtc::ice_transport::ice_candidate::RTCIceCandidateInit,
    ) -> Result<()> {
        self.peer_connection
            .add_ice_candidate(candidate)
            .await
            .context("Failed to add ICE candidate")
    }

    /// Get the connection state
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.peer_connection.connection_state()
    }

    /// Get the ICE connection state
    #[allow(dead_code)]
    pub fn ice_connection_state(&self) -> RTCIceConnectionState {
        self.peer_connection.ice_connection_state()
    }

    /// Get connection info (candidate type, addresses, etc.)
    pub async fn get_connection_info(&self) -> WebRtcConnectionInfo {
        let stats = self.peer_connection.get_stats().await;

        let mut local_candidate_type: Option<CandidateType> = None;
        let mut remote_candidate_type: Option<CandidateType> = None;
        let mut local_address = None;
        let mut remote_address = None;
        let mut nominated_pair_local_id = None;
        let mut nominated_pair_remote_id = None;

        // First pass: find the nominated candidate pair
        for report in stats.reports.values() {
            if let StatsReportType::CandidatePair(pair) = report
                && pair.nominated
            {
                nominated_pair_local_id = Some(pair.local_candidate_id.clone());
                nominated_pair_remote_id = Some(pair.remote_candidate_id.clone());
                break;
            }
        }

        // Second pass: get candidate details
        for (id, report) in &stats.reports {
            match report {
                StatsReportType::LocalCandidate(candidate)
                    if nominated_pair_local_id.as_ref() == Some(id) =>
                {
                    local_candidate_type = Some(candidate.candidate_type);
                    local_address = Some(format!("{}:{}", candidate.ip, candidate.port));
                }
                StatsReportType::RemoteCandidate(candidate)
                    if nominated_pair_remote_id.as_ref() == Some(id) =>
                {
                    remote_candidate_type = Some(candidate.candidate_type);
                    remote_address = Some(format!("{}:{}", candidate.ip, candidate.port));
                }
                _ => {}
            }
        }

        // Determine connection type based on candidate types
        let connection_type = match (local_candidate_type, remote_candidate_type) {
            (Some(local), Some(remote)) => {
                // If either side uses a relay, the connection is relayed
                if matches!(local, CandidateType::Relay) || matches!(remote, CandidateType::Relay) {
                    "Relay (TURN)".to_string()
                } else if matches!(local, CandidateType::Host)
                    && matches!(remote, CandidateType::Host)
                {
                    "Direct (Host)".to_string()
                } else if matches!(local, CandidateType::ServerReflexive)
                    || matches!(remote, CandidateType::ServerReflexive)
                {
                    "Direct (STUN)".to_string()
                } else if matches!(local, CandidateType::PeerReflexive)
                    || matches!(remote, CandidateType::PeerReflexive)
                {
                    "Direct (Peer Reflexive)".to_string()
                } else {
                    // Fallback for Unspecified or any future variants
                    format!(
                        "Unknown ({}/{})",
                        candidate_type_to_str(local),
                        candidate_type_to_str(remote)
                    )
                }
            }
            _ => "Unknown".to_string(),
        };

        WebRtcConnectionInfo {
            connection_type,
            local_address,
            remote_address,
        }
    }

    /// Close the peer connection
    pub async fn close(&self) -> Result<()> {
        self.peer_connection
            .close()
            .await
            .context("Failed to close peer connection")
    }
}

/// WebRTC connection information
#[derive(Debug, Clone)]
pub struct WebRtcConnectionInfo {
    pub connection_type: String,
    pub local_address: Option<String>,
    pub remote_address: Option<String>,
}

// ============================================================================
// DcMessenger - message-oriented adapter over a detached data channel
// ============================================================================
//
// secure-send-web sends each 128 KiB encrypted chunk as its own binary
// data-channel message and control signals (DONE:N:B / ACK) as string messages.
// The webrtc facade's built-in reader caps messages at 65535 bytes, so we
// detach the channel (see `open_and_detach`) and run our own read loop with a
// buffer large enough for a full chunk, preserving message boundaries.

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;
use webrtc::data::data_channel::DataChannel as RawDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

/// Receive buffer size for one data-channel message. Matches the
/// `max-message-size` we advertise (262144), comfortably above a 128 KiB chunk.
const RECV_BUFFER_SIZE: usize = 256 * 1024;

/// One received data-channel message: string vs binary is significant to the
/// transfer protocol (control signals are strings, chunks are binary).
#[derive(Debug, Clone)]
pub struct DcMessage {
    pub is_string: bool,
    pub data: Bytes,
}

/// Message-oriented wrapper over a detached [`RawDataChannel`].
pub struct DcMessenger {
    raw: Arc<RawDataChannel>,
    message_rx: mpsc::UnboundedReceiver<DcMessage>,
    closed: Arc<AtomicBool>,
}

impl DcMessenger {
    /// Wrap a detached data channel, spawning a read loop that forwards
    /// discrete messages.
    pub fn new(raw: Arc<RawDataChannel>) -> Self {
        let (tx, message_rx) = mpsc::unbounded_channel::<DcMessage>();
        let closed = Arc::new(AtomicBool::new(false));

        let read_raw = raw.clone();
        let read_closed = closed.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; RECV_BUFFER_SIZE];
            loop {
                match read_raw.read_data_channel(&mut buffer).await {
                    Ok((0, _)) => break, // stream reset / closed
                    Ok((n, is_string)) => {
                        let msg = DcMessage {
                            is_string,
                            data: Bytes::copy_from_slice(&buffer[..n]),
                        };
                        if tx.send(msg).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) => {
                        log::debug!("data channel read ended: {e}");
                        break;
                    }
                }
            }
            read_closed.store(true, Ordering::SeqCst);
        });

        Self {
            raw,
            message_rx,
            closed,
        }
    }

    /// Receive the next message, or `None` once the channel is closed/drained.
    pub async fn recv(&mut self) -> Option<DcMessage> {
        self.message_rx.recv().await
    }

    /// Send a binary message (one encrypted chunk).
    pub async fn send_binary(&self, data: Bytes) -> Result<()> {
        self.raw
            .write_data_channel(&data, false)
            .await
            .context("Failed to send binary message")?;
        Ok(())
    }

    /// Send a text message (a control signal such as `DONE:N:B` or `ACK`).
    pub async fn send_text(&self, text: impl Into<String>) -> Result<()> {
        let bytes = Bytes::from(text.into().into_bytes());
        self.raw
            .write_data_channel(&bytes, true)
            .await
            .context("Failed to send text message")?;
        Ok(())
    }

    /// Number of bytes queued in the SCTP send buffer (for backpressure).
    pub fn buffered_amount(&self) -> usize {
        self.raw.buffered_amount()
    }

    /// Whether the read loop has observed the channel closing.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Wait for a data channel to open, then detach it for direct large-message
/// I/O. Requires `detach_data_channels()` on the setting engine.
pub async fn open_and_detach(
    data_channel: Arc<RTCDataChannel>,
    timeout: Duration,
) -> Result<Arc<RawDataChannel>> {
    // Fast path: already open (e.g. an incoming channel that opened before we
    // attached a handler).
    if data_channel.ready_state() == RTCDataChannelState::Open {
        return data_channel
            .detach()
            .await
            .context("Failed to detach data channel");
    }

    let (tx, rx) = oneshot::channel();
    let dc = data_channel.clone();
    data_channel.on_open(Box::new(move || {
        let dc = dc.clone();
        Box::pin(async move {
            let _ = tx.send(dc.detach().await);
        })
    }));

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Ok(raw))) => Ok(raw),
        Ok(Ok(Err(e))) => Err(e).context("Failed to detach data channel"),
        Ok(Err(_)) => anyhow::bail!("Data channel open signal was cancelled"),
        Err(_) => anyhow::bail!("Timed out waiting for the data channel to open"),
    }
}
