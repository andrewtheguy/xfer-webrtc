//! Sans-I/O WebRTC peer and data-channel adapter.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use rtc::data_channel::RTCDataChannelId;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::peer_connection::configuration::setting_engine::{SctpMaxMessageSize, SettingEngine};
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::{RTCIceConnectionState, RTCPeerConnectionState};
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, CandidateServerReflexiveConfig, RTCIceCandidate,
    RTCIceCandidateInit,
};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc, oneshot, watch};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const DATAGRAM_BUFFER_SIZE: usize = 65_536;
const BUFFERED_AMOUNT_LOW: u32 = 512 * 1024;
const BUFFERED_AMOUNT_HIGH: u32 = 1024 * 1024;
const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

enum PeerCommand {
    CreateDataChannel {
        label: String,
        response: oneshot::Sender<std::result::Result<ChannelParts, String>>,
    },
    CreateOffer(oneshot::Sender<std::result::Result<RTCSessionDescription, String>>),
    CreateAnswer(oneshot::Sender<std::result::Result<RTCSessionDescription, String>>),
    SetLocal {
        description: RTCSessionDescription,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    SetRemote {
        description: RTCSessionDescription,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    AddRemoteCandidate {
        candidate: RTCIceCandidateInit,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    Send {
        channel_id: RTCDataChannelId,
        data: Bytes,
        is_string: bool,
        response: oneshot::Sender<std::result::Result<(), String>>,
    },
    Close(oneshot::Sender<std::result::Result<(), String>>),
}

struct ChannelParts {
    id: RTCDataChannelId,
    label: String,
    message_rx: mpsc::UnboundedReceiver<DcMessage>,
    opened_rx: watch::Receiver<bool>,
    closed: Arc<AtomicBool>,
    send_allowed: Arc<AtomicBool>,
    send_ready: Arc<Notify>,
}

struct WorkerChannel {
    message_tx: mpsc::UnboundedSender<DcMessage>,
    opened_tx: watch::Sender<bool>,
    closed: Arc<AtomicBool>,
    send_allowed: Arc<AtomicBool>,
    send_ready: Arc<Notify>,
}

struct WorkerContext {
    command_tx: mpsc::Sender<PeerCommand>,
    data_channel_tx: mpsc::Sender<RtcDataChannel>,
    connection_state: Arc<RwLock<RTCPeerConnectionState>>,
    ice_connection_state: Arc<RwLock<RTCIceConnectionState>>,
    connection_info: Arc<RwLock<WebRtcConnectionInfo>>,
}

fn new_channel_parts(
    id: RTCDataChannelId,
    label: String,
) -> (ChannelParts, WorkerChannel) {
    let (message_tx, message_rx) = mpsc::unbounded_channel();
    let (opened_tx, opened_rx) = watch::channel(false);
    let closed = Arc::new(AtomicBool::new(false));
    let send_allowed = Arc::new(AtomicBool::new(true));
    let send_ready = Arc::new(Notify::new());
    (
        ChannelParts {
            id,
            label: label.clone(),
            message_rx,
            opened_rx,
            closed: closed.clone(),
            send_allowed: send_allowed.clone(),
            send_ready: send_ready.clone(),
        },
        WorkerChannel {
            message_tx,
            opened_tx,
            closed,
            send_allowed,
            send_ready,
        },
    )
}

/// WebRTC peer connection driven by a Tokio UDP event loop.
pub struct WebRtcPeer {
    command_tx: mpsc::Sender<PeerCommand>,
    candidates: Vec<RTCIceCandidateInit>,
    data_channel_rx: Option<mpsc::Receiver<RtcDataChannel>>,
    connection_state: Arc<RwLock<RTCPeerConnectionState>>,
    ice_connection_state: Arc<RwLock<RTCIceConnectionState>>,
    connection_info: Arc<RwLock<WebRtcConnectionInfo>>,
}

impl WebRtcPeer {
    pub async fn new() -> Result<Self> {
        let bind_ip = discover_local_ip();
        let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0))
            .await
            .context("Failed to bind WebRTC UDP socket")?;
        let local_addr = socket.local_addr()?;

        let host_candidate = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: local_addr.ip().to_string(),
                port: local_addr.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()
        .context("Failed to create local ICE candidate")?;
        let host_candidate = RTCIceCandidate::from(&host_candidate)
            .to_json()
            .context("Failed to serialize local ICE candidate")?;

        let mut candidates = vec![host_candidate];
        for (mapped_addr, stun_server) in gather_server_reflexive_candidates(&socket).await {
            let candidate = CandidateServerReflexiveConfig {
                base_config: CandidateConfig {
                    network: "udp".to_owned(),
                    address: mapped_addr.ip().to_string(),
                    port: mapped_addr.port(),
                    component: 1,
                    ..Default::default()
                },
                rel_addr: local_addr.ip().to_string(),
                rel_port: local_addr.port(),
                url: Some(format!("stun:{stun_server}")),
            }
            .new_candidate_server_reflexive()
            .context("Failed to create server-reflexive ICE candidate")?;
            let mut candidate = RTCIceCandidate::from(&candidate)
                .to_json()
                .context("Failed to serialize server-reflexive ICE candidate")?;
            candidate.url = Some(format!("stun:{stun_server}"));
            if !candidates
                .iter()
                .any(|existing| existing.candidate == candidate.candidate)
            {
                candidates.push(candidate);
            }
        }

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_sctp_max_message_size(SctpMaxMessageSize::Unbounded);
        let mut peer_connection = RTCPeerConnectionBuilder::new()
            .with_setting_engine(setting_engine)
            .build()
            .context("Failed to create peer connection")?;
        for candidate in &candidates {
            peer_connection
                .add_local_candidate(candidate.clone())
                .context("Failed to add local ICE candidate")?;
        }

        let (command_tx, command_rx) = mpsc::channel(32);
        let (data_channel_tx, data_channel_rx) = mpsc::channel(1);
        let connection_state = Arc::new(RwLock::new(RTCPeerConnectionState::New));
        let ice_connection_state = Arc::new(RwLock::new(RTCIceConnectionState::New));
        let connection_info = Arc::new(RwLock::new(WebRtcConnectionInfo {
            connection_type: "Unknown".to_owned(),
            local_address: Some(local_addr.to_string()),
            remote_address: None,
        }));

        tokio::spawn(run_peer(peer_connection, socket, command_rx, WorkerContext {
            command_tx: command_tx.clone(),
            data_channel_tx,
            connection_state: connection_state.clone(),
            ice_connection_state: ice_connection_state.clone(),
            connection_info: connection_info.clone(),
        }));

        Ok(Self {
            command_tx,
            candidates,
            data_channel_rx: Some(data_channel_rx),
            connection_state,
            ice_connection_state,
            connection_info,
        })
    }

    pub fn take_data_channel_rx(&mut self) -> Option<mpsc::Receiver<RtcDataChannel>> {
        self.data_channel_rx.take()
    }

    pub async fn gather_ice_candidates(
        &mut self,
        _timeout: Duration,
    ) -> Result<Vec<RTCIceCandidateInit>> {
        let public_candidate = self
            .candidates
            .iter()
            .any(|candidate| candidate.candidate.contains(" typ srflx"));
        crate::ui::status(&format!(
            "Gathered {} network candidate(s){}.",
            self.candidates.len(),
            if public_candidate {
                ", including a public address"
            } else {
                "; public-address discovery failed"
            }
        ));
        Ok(self.candidates.clone())
    }

    pub async fn create_data_channel(&self, label: &str) -> Result<RtcDataChannel> {
        let (response, rx) = oneshot::channel();
        self.command_tx
            .send(PeerCommand::CreateDataChannel {
                label: label.to_owned(),
                response,
            })
            .await
            .context("Peer connection closed")?;
        let parts = receive_response(rx, "create data channel").await?;
        eprintln!("Created data channel: {label}");
        Ok(RtcDataChannel::new(parts, self.command_tx.clone()))
    }

    pub async fn create_offer(&self) -> Result<RTCSessionDescription> {
        let (tx, rx) = oneshot::channel();
        self.command_tx.send(PeerCommand::CreateOffer(tx)).await?;
        receive_response(rx, "create offer").await
    }

    pub async fn create_answer(&self) -> Result<RTCSessionDescription> {
        let (tx, rx) = oneshot::channel();
        self.command_tx.send(PeerCommand::CreateAnswer(tx)).await?;
        receive_response(rx, "create answer").await
    }

    pub async fn set_local_description(&self, description: RTCSessionDescription) -> Result<()> {
        let (response, rx) = oneshot::channel();
        self.command_tx
            .send(PeerCommand::SetLocal {
                description,
                response,
            })
            .await?;
        receive_response(rx, "set local description").await
    }

    pub async fn set_remote_description(&self, description: RTCSessionDescription) -> Result<()> {
        let (response, rx) = oneshot::channel();
        self.command_tx
            .send(PeerCommand::SetRemote {
                description,
                response,
            })
            .await?;
        receive_response(rx, "set remote description").await
    }

    pub async fn add_ice_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        let (response, rx) = oneshot::channel();
        self.command_tx
            .send(PeerCommand::AddRemoteCandidate {
                candidate,
                response,
            })
            .await?;
        receive_response(rx, "add ICE candidate").await
    }

    pub fn connection_state(&self) -> RTCPeerConnectionState {
        *self.connection_state.read().expect("connection state poisoned")
    }

    #[allow(dead_code)]
    pub fn ice_connection_state(&self) -> RTCIceConnectionState {
        *self
            .ice_connection_state
            .read()
            .expect("ICE connection state poisoned")
    }

    pub async fn get_connection_info(&self) -> WebRtcConnectionInfo {
        self.connection_info
            .read()
            .expect("connection info poisoned")
            .clone()
    }

    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(PeerCommand::Close(tx)).await.is_err() {
            return Ok(());
        }
        receive_response(rx, "close peer connection").await
    }
}

async fn receive_response<T>(
    rx: oneshot::Receiver<std::result::Result<T, String>>,
    operation: &str,
) -> Result<T> {
    rx.await
        .with_context(|| format!("Peer connection stopped while trying to {operation}"))?
        .map_err(anyhow::Error::msg)
}

async fn run_peer(
    mut peer: rtc::peer_connection::RTCPeerConnection,
    socket: UdpSocket,
    mut command_rx: mpsc::Receiver<PeerCommand>,
    context: WorkerContext,
) {
    let local_addr = match socket.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            log::error!("WebRTC socket has no local address: {error}");
            return;
        }
    };
    let mut channels = HashMap::<RTCDataChannelId, WorkerChannel>::new();
    let mut buffer = vec![0; DATAGRAM_BUFFER_SIZE];

    'event_loop: loop {
        while let Some(transmit) = peer.poll_write() {
            if let Err(error) = socket
                .send_to(&transmit.message, transmit.transport.peer_addr)
                .await
            {
                log::warn!("WebRTC UDP write failed: {error}");
            }
        }

        while let Some(event) = peer.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    *context.connection_state.write().expect("connection state poisoned") = state;
                    match state {
                        RTCPeerConnectionState::Connected => {
                            crate::ui::status("WebRTC connection established!");
                        }
                        RTCPeerConnectionState::Disconnected => {
                            crate::ui::status("WebRTC connection disconnected");
                        }
                        RTCPeerConnectionState::Failed => log::error!("WebRTC connection failed"),
                        RTCPeerConnectionState::Closed => {
                            crate::ui::status("WebRTC connection closed");
                        }
                        _ => {}
                    }
                }
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                    *context.ice_connection_state
                        .write()
                        .expect("ICE connection state poisoned") = state;
                    match state {
                        RTCIceConnectionState::Checking => {
                            crate::ui::status("Checking direct network routes...");
                        }
                        RTCIceConnectionState::Failed => {
                            log::error!("ICE failed: no direct network route reached the peer");
                        }
                        _ => {}
                    }
                }
                RTCPeerConnectionEvent::OnDataChannel(channel_event) => match channel_event {
                    RTCDataChannelEvent::OnOpen(id) => {
                        if let Some(channel) = channels.get(&id) {
                            let _ = channel.opened_tx.send(true);
                        } else if let Some(mut dc) = peer.data_channel(id) {
                            let label = dc.label().to_owned();
                            dc.set_buffered_amount_low_threshold(BUFFERED_AMOUNT_LOW);
                            dc.set_buffered_amount_high_threshold(BUFFERED_AMOUNT_HIGH);
                            let (parts, worker) = new_channel_parts(id, label);
                            let _ = worker.opened_tx.send(true);
                            channels.insert(id, worker);
                            if context.data_channel_tx
                                .send(RtcDataChannel::new(parts, context.command_tx.clone()))
                                .await
                                .is_err()
                            {
                                log::trace!("Incoming data-channel receiver closed");
                            }
                        }
                    }
                    RTCDataChannelEvent::OnClose(id) | RTCDataChannelEvent::OnError(id) => {
                        if let Some(channel) = channels.get(&id) {
                            channel.closed.store(true, Ordering::SeqCst);
                            channel.send_ready.notify_waiters();
                        }
                    }
                    RTCDataChannelEvent::OnBufferedAmountHigh(id) => {
                        if let Some(channel) = channels.get(&id) {
                            channel.send_allowed.store(false, Ordering::SeqCst);
                        }
                    }
                    RTCDataChannelEvent::OnBufferedAmountLow(id) => {
                        if let Some(channel) = channels.get(&id) {
                            channel.send_allowed.store(true, Ordering::SeqCst);
                            channel.send_ready.notify_waiters();
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        while let Some(message) = peer.poll_read() {
            if let RTCMessage::DataChannelMessage(id, message) = message
                && let Some(channel) = channels.get(&id)
            {
                let _ = channel.message_tx.send(DcMessage {
                    is_string: message.is_string,
                    data: message.data.freeze(),
                });
            }
        }

        let timeout = peer
            .poll_timeout()
            .unwrap_or_else(|| Instant::now() + DEFAULT_TIMEOUT);
        let delay = timeout.saturating_duration_since(Instant::now());
        if delay.is_zero() {
            if let Err(error) = peer.handle_timeout(Instant::now()) {
                log::error!("WebRTC timer failed: {error}");
                break;
            }
            continue;
        }
        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        tokio::select! {
            command = command_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    PeerCommand::CreateDataChannel { label, response } => {
                        let result = peer.create_data_channel(&label, None)
                            .map(|mut dc| {
                                dc.set_buffered_amount_low_threshold(BUFFERED_AMOUNT_LOW);
                                dc.set_buffered_amount_high_threshold(BUFFERED_AMOUNT_HIGH);
                                (dc.id(), dc.label().to_owned())
                            })
                            .map_err(|error| error.to_string())
                            .map(|(id, label)| {
                                let (parts, worker) = new_channel_parts(id, label);
                                channels.insert(id, worker);
                                parts
                            });
                        let _ = response.send(result);
                    }
                    PeerCommand::CreateOffer(response) => {
                        let _ = response.send(peer.create_offer(None).map_err(|e| e.to_string()));
                    }
                    PeerCommand::CreateAnswer(response) => {
                        let _ = response.send(peer.create_answer(None).map_err(|e| e.to_string()));
                    }
                    PeerCommand::SetLocal { description, response } => {
                        let _ = response.send(peer.set_local_description(description).map_err(|e| e.to_string()));
                    }
                    PeerCommand::SetRemote { description, response } => {
                        let _ = response.send(peer.set_remote_description(description).map_err(|e| e.to_string()));
                    }
                    PeerCommand::AddRemoteCandidate { candidate, response } => {
                        let _ = response.send(peer.add_remote_candidate(candidate).map_err(|e| e.to_string()));
                    }
                    PeerCommand::Send { channel_id, data, is_string, response } => {
                        let result = peer.data_channel(channel_id)
                            .ok_or_else(|| "data channel is closed".to_owned())
                            .and_then(|mut dc| {
                                if is_string {
                                    let text = String::from_utf8(data.to_vec())
                                        .map_err(|e| e.to_string())?;
                                    dc.send_text(text).map_err(|e| e.to_string())
                                } else {
                                    dc.send(BytesMut::from(data.as_ref())).map_err(|e| e.to_string())
                                }
                            });
                        let _ = response.send(result);
                    }
                    PeerCommand::Close(response) => {
                        let result = peer.close().map_err(|e| e.to_string());
                        let _ = response.send(result);
                        break 'event_loop;
                    }
                }
            }
            result = socket.recv_from(&mut buffer) => {
                match result {
                    Ok((size, remote_addr)) => {
                        {
                            let mut info = context.connection_info.write().expect("connection info poisoned");
                            info.connection_type = "Direct (Host)".to_owned();
                            info.remote_address = Some(remote_addr.to_string());
                        }
                        if let Err(error) = peer.handle_read(TaggedBytesMut {
                            now: Instant::now(),
                            transport: TransportContext {
                                local_addr,
                                peer_addr: remote_addr,
                                ecn: None,
                                transport_protocol: TransportProtocol::UDP,
                            },
                            message: BytesMut::from(&buffer[..size]),
                        }) {
                            log::debug!("Ignoring invalid WebRTC datagram: {error}");
                        }
                    }
                    Err(error) => {
                        log::error!("WebRTC UDP read failed: {error}");
                        break;
                    }
                }
            }
            _ = timer.as_mut() => {
                if let Err(error) = peer.handle_timeout(Instant::now()) {
                    log::error!("WebRTC timer failed: {error}");
                    break;
                }
            }
        }
    }

    for channel in channels.values() {
        channel.closed.store(true, Ordering::SeqCst);
        channel.send_ready.notify_waiters();
    }
}

fn discover_local_ip() -> IpAddr {
    let fallback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let Ok(socket) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return fallback;
    };
    if socket.connect((Ipv4Addr::new(1, 1, 1, 1), 3478)).is_err() {
        return fallback;
    }
    socket.local_addr().map(|addr| addr.ip()).unwrap_or(fallback)
}

async fn gather_server_reflexive_candidates(socket: &UdpSocket) -> Vec<(SocketAddr, String)> {
    let Ok(local_addr) = socket.local_addr() else {
        return Vec::new();
    };
    let local_is_ipv4 = local_addr.is_ipv4();
    let mut candidates = Vec::new();
    for server in STUN_SERVERS {
        let Ok(Ok(resolved)) = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::lookup_host(*server),
        )
        .await
        else {
            continue;
        };
        let Some(server_addr) = resolved
            .into_iter()
            .find(|addr| addr.is_ipv4() == local_is_ipv4)
        else {
            continue;
        };

        let mut transaction_id = [0u8; 12];
        if getrandom::getrandom(&mut transaction_id).is_err() {
            return candidates;
        }
        let mut request = [0u8; 20];
        request[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
        request[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
        request[8..20].copy_from_slice(&transaction_id);
        if socket.send_to(&request, server_addr).await.is_err() {
            continue;
        }

        let mut response = [0u8; 1500];
        let received = tokio::time::timeout(
            Duration::from_millis(700),
            socket.recv_from(&mut response),
        )
        .await;
        let Ok(Ok((length, source))) = received else {
            continue;
        };
        if source != server_addr {
            continue;
        }
        if let Some(mapped_addr) = parse_stun_binding_response(
            &response[..length],
            &transaction_id,
        ) && !candidates
                .iter()
                .any(|(existing, _)| *existing == mapped_addr)
        {
            candidates.push((mapped_addr, (*server).to_owned()));
        }
    }
    candidates
}

fn parse_stun_binding_response(packet: &[u8], transaction_id: &[u8; 12]) -> Option<SocketAddr> {
    const MAGIC_COOKIE: u32 = 0x2112_A442;
    if packet.len() < 20
        || u16::from_be_bytes([packet[0], packet[1]]) != 0x0101
        || packet[4..8] != MAGIC_COOKIE.to_be_bytes()
        || packet[8..20] != transaction_id[..]
    {
        return None;
    }

    let attributes_length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let end = 20usize.checked_add(attributes_length)?.min(packet.len());
    let mut offset: usize = 20;
    while offset.checked_add(4)? <= end {
        let attribute_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let length = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.checked_add(length)?;
        if value_end > end {
            return None;
        }
        if attribute_type == 0x0020 && length >= 8 {
            let value = &packet[value_start..value_end];
            let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
            return match value[1] {
                0x01 if length >= 8 => {
                    let encoded = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                    Some(SocketAddr::new(IpAddr::V4((encoded ^ MAGIC_COOKIE).into()), port))
                }
                0x02 if length >= 20 => {
                    let mut mask = [0u8; 16];
                    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                    mask[4..].copy_from_slice(transaction_id);
                    let mut address = [0u8; 16];
                    for (index, byte) in address.iter_mut().enumerate() {
                        *byte = value[4 + index] ^ mask[index];
                    }
                    Some(SocketAddr::new(IpAddr::V6(address.into()), port))
                }
                _ => None,
            };
        }
        offset = value_start + length.div_ceil(4) * 4;
    }
    None
}

#[derive(Debug, Clone)]
pub struct WebRtcConnectionInfo {
    pub connection_type: String,
    pub local_address: Option<String>,
    pub remote_address: Option<String>,
}

/// Handle for one `rtc` data channel.
pub struct RtcDataChannel {
    id: RTCDataChannelId,
    #[allow(dead_code)]
    label: String,
    command_tx: mpsc::Sender<PeerCommand>,
    message_rx: mpsc::UnboundedReceiver<DcMessage>,
    opened_rx: watch::Receiver<bool>,
    closed: Arc<AtomicBool>,
    send_allowed: Arc<AtomicBool>,
    send_ready: Arc<Notify>,
}

impl RtcDataChannel {
    fn new(parts: ChannelParts, command_tx: mpsc::Sender<PeerCommand>) -> Self {
        Self {
            id: parts.id,
            label: parts.label,
            command_tx,
            message_rx: parts.message_rx,
            opened_rx: parts.opened_rx,
            closed: parts.closed,
            send_allowed: parts.send_allowed,
            send_ready: parts.send_ready,
        }
    }

    async fn send(&self, data: Bytes, is_string: bool) -> Result<()> {
        loop {
            let notified = self.send_ready.notified();
            if self.closed.load(Ordering::SeqCst) {
                bail!("data channel is closed");
            }
            if self.send_allowed.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
        let (response, rx) = oneshot::channel();
        self.command_tx
            .send(PeerCommand::Send {
                channel_id: self.id,
                data,
                is_string,
                response,
            })
            .await
            .context("Peer connection closed")?;
        receive_response(rx, "send data-channel message").await
    }
}

#[derive(Debug, Clone)]
pub struct DcMessage {
    pub is_string: bool,
    pub data: Bytes,
}

/// Message-oriented transfer adapter over an `rtc` data channel.
pub struct DcMessenger {
    channel: RtcDataChannel,
}

impl DcMessenger {
    pub fn new(channel: RtcDataChannel) -> Self {
        Self { channel }
    }

    pub async fn recv(&mut self) -> Option<DcMessage> {
        self.channel.message_rx.recv().await
    }

    pub async fn send_binary(&self, data: Bytes) -> Result<()> {
        self.channel.send(data, false).await
    }

    pub async fn send_text(&self, text: impl Into<String>) -> Result<()> {
        self.channel
            .send(Bytes::from(text.into().into_bytes()), true)
            .await
    }

    pub fn buffered_amount(&self) -> usize {
        0
    }

    pub fn is_closed(&self) -> bool {
        self.channel.closed.load(Ordering::SeqCst)
    }
}

/// Wait until the sans-I/O worker reports that a channel is open.
pub async fn open_and_detach(
    mut data_channel: RtcDataChannel,
    timeout: Duration,
) -> Result<RtcDataChannel> {
    if *data_channel.opened_rx.borrow() {
        return Ok(data_channel);
    }

    tokio::time::timeout(timeout, async {
        loop {
            if data_channel.opened_rx.changed().await.is_err() {
                bail!("Data channel open signal was cancelled");
            }
            if *data_channel.opened_rx.borrow() {
                return Ok(data_channel);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Timed out waiting for the data channel to open"))?
}
