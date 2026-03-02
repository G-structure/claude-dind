//! WebSocket-to-byte-stream bridge for the Cloudflare Worker relay.
//!
//! # The problem
//!
//! When both the server and agent are behind NAT, they need a relay. The TCP
//! relay (`gwp relay`) works but requires a VPS. A Cloudflare Worker with a
//! Durable Object can do the same job serverlessly — but Workers communicate
//! via WebSocket, not raw TCP.
//!
//! The rest of our stack (TLS, auth, yamux) expects a byte stream (`AsyncRead +
//! AsyncWrite`). WebSocket is message-based (discrete frames, not a continuous
//! byte stream). This module bridges the gap.
//!
//! # How the bridge works
//!
//! ```text
//! [tunnel code]                [WebSocket]              [CF Worker DO]
//!      │                           │                          │
//!      ▼ write                     │                          │
//! DuplexStream ──▶ pump task ──▶ WS Binary frame ──────────▶  │
//!      │                           │                          │
//!      ▲ read                      │                          │
//! DuplexStream ◀── pump task ◀── WS Binary frame ◀──────────  │
//! ```
//!
//! We create a `tokio::io::DuplexStream` pair — an in-memory pipe where each
//! end implements `AsyncRead + AsyncWrite`. The "local" end is returned to the
//! caller (who passes it to TLS/yamux). The "remote" end is connected to two
//! background pump tasks:
//!
//! - **WS → duplex**: Reads `Binary` frames from the WebSocket, writes the raw
//!   bytes into the duplex stream. Ignores `Text`, `Ping`, `Pong` frames.
//!   Exits on `Close` or error.
//!
//! - **duplex → WS**: Reads bytes from the duplex stream into a 64KB buffer,
//!   wraps each chunk as a `Binary` frame, sends it over the WebSocket. Exits
//!   when the duplex stream closes (read returns 0) or on error.
//!
//! # Buffer sizing
//!
//! - **DuplexStream: 256KB** — Large enough to absorb bursts without back-pressure
//!   stalling the pump tasks. yamux has its own 256KB default window, so this
//!   matches well.
//! - **Read buffer: 64KB** — Matches typical TCP MSS multiples and WebSocket frame
//!   sizes. Larger buffers would waste memory; smaller ones would increase
//!   syscall overhead.
//!
//! # Why `native-tls` for tungstenite
//!
//! `tokio-tungstenite` needs a TLS backend for `wss://` URLs. We use the
//! `native-tls` feature (which uses the platform's TLS — SecureTransport on
//! macOS, OpenSSL on Linux). We can't reuse our `rustls` + `ring` setup because
//! `tokio-tungstenite`'s rustls integration pulls in `aws-lc-rs` by default,
//! which requires a C compiler — the exact thing we're avoiding. The `native-tls`
//! backend uses the system trust store, which is fine here because we're
//! connecting to Cloudflare's edge (a real CA-signed cert), not to our
//! self-signed tunnel cert.

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

/// Connect to a WebSocket URL and return a byte-stream interface.
///
/// The returned `DuplexStream` implements `tokio::io::AsyncRead + AsyncWrite`.
/// Two background tasks pump bytes between it and the WebSocket. When the
/// WebSocket closes or errors, the tasks exit and the duplex stream sees EOF.
pub async fn connect(url: &str) -> Result<DuplexStream> {
    let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
    debug!("WebSocket connected to {url}");
    Ok(bridge(ws))
}

/// Create a DuplexStream bridged to a WebSocket via two pump tasks.
///
/// Returns the "local" end of the duplex. The "remote" end is split into a
/// reader and writer, each driven by a spawned task that translates between
/// byte-stream reads/writes and WebSocket Binary frames.
fn bridge<S>(ws: tokio_tungstenite::WebSocketStream<S>) -> DuplexStream
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (local, remote) = io::duplex(256 * 1024);
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (mut rd, mut wr) = io::split(remote);

    // WS → duplex: read Binary frames, write raw bytes to the duplex pipe.
    tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if wr.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        debug!("ws→duplex pump exited");
    });

    // duplex → WS: read bytes from the duplex pipe, send as Binary frames.
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_sink
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = ws_sink.close().await;
        debug!("duplex→ws pump exited");
    });

    local
}
