//! beam-rs-webrtc: WebRTC transport for peer-to-peer file transfer.
//!
//! This crate provides the core transfer/crypto/signaling functionality along
//! with the WebRTC data-channel transport. The `beam-rs-webrtc` binary is a
//! thin wrapper over this library.

pub mod core;
pub mod signaling;
pub mod ui;
pub mod webrtc;
