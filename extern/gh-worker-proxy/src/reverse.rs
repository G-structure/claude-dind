//! Reverse port forwarding through the yamux tunnel.
//!
//! The local machine needs to issue Docker commands against the remote runner's
//! Docker daemon. gwp's yamux connection is bidirectional — this module adds
//! server→agent stream opening for reverse port forwarding.
//!
//! # Protocol
//!
//! Server opens a yamux outbound stream and writes `[0xFF, ATYP, ADDR, PORT]`.
//! The `0xFF` prefix distinguishes reverse streams from regular SOCKS5 streams
//! (which start with ATYP 0x01/0x03/0x04).
//!
//! On the agent side, when an inbound stream starts with `0xFF`, the agent reads
//! the target address and connects to it locally, then copies bytes bidirectionally.

use anyhow::Result;
use tokio::sync::oneshot;
use yamux::Stream as YamuxStream;

use crate::socks5::Address;

/// Prefix byte that distinguishes reverse-forwarded streams from SOCKS5 streams.
pub const REVERSE_PREFIX: u8 = 0xFF;

/// A request from the local side to open a reverse-forwarded connection
/// to a target address on the agent's network.
pub struct ReverseForwardRequest {
    /// The target address to connect to on the agent side.
    pub target: Address,
    /// Channel to return the yamux stream (or error) once the outbound stream is opened.
    pub respond: oneshot::Sender<Result<YamuxStream>>,
}
