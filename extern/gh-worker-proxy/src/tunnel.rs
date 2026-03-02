//! Token authentication and yamux multiplexing over TLS.
//!
//! This module handles two concerns:
//!
//! 1. **Auth handshake**: A simple token exchange that happens over the raw TLS stream
//!    *before* yamux starts. The client sends a token; the server validates it. This
//!    prevents unauthorized use of the tunnel even if someone discovers the server's
//!    address and fingerprint.
//!
//! 2. **yamux driver**: yamux is a stream multiplexer — one TCP/TLS connection carries
//!    many logical streams. Each SOCKS5 connection becomes one yamux stream. The tricky
//!    part is that yamux uses `futures::io` traits (not tokio's), and its
//!    `poll_next_inbound` is **not cancel-safe**.
//!
//! # The cancel-safety problem
//!
//! `yamux::Connection::poll_next_inbound` maintains internal state between polls.
//! If you use it inside `tokio::select!` and the other branch fires, the in-progress
//! poll is dropped, potentially losing data. The fix: a single `poll_fn` that drives
//! *all* yamux operations without any `select!`.
//!
//! # The `futures::io` vs `tokio::io` problem
//!
//! yamux's `Connection<T>` requires `T: futures::io::AsyncRead + futures::io::AsyncWrite`.
//! But tokio-rustls streams implement `tokio::io::AsyncRead + tokio::io::AsyncWrite`.
//! We bridge them with `tokio_util::compat::TokioAsyncReadCompatExt`, which wraps
//! a tokio stream in a `Compat<S>` adapter implementing the futures traits.

use std::future::{poll_fn, Future};
use std::task::Poll;

use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{debug, error, info, warn};
use yamux::{Config, Connection, Mode, Stream as YamuxStream};

use crate::reverse::{self, ReverseForwardRequest};
use crate::socks5::Address;

// ── Auth protocol ───────────────────────────────────────────────────
//
// Runs over raw TLS, before yamux. Intentionally simple:
//   client → server: [len: u8] [token: [u8; len]]
//   server → client: [0x00] (OK) or [0x01] (rejected)
//
// The explicit flush() after each write is critical — tokio-rustls buffers
// like BufWriter, so without flush the bytes sit in the buffer and the
// other side hangs waiting.

/// Client side of the auth handshake. Sends the token and waits for OK.
pub async fn auth_client<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    token: &str,
) -> Result<()> {
    let bytes = token.as_bytes();
    stream.write_u8(bytes.len() as u8).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    let resp = stream.read_u8().await?;
    if resp != 0x00 {
        bail!("auth rejected (code {resp:#x})");
    }
    Ok(())
}

/// Server side of the auth handshake. Reads the token and validates it.
pub async fn auth_server<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    expected: &str,
) -> Result<()> {
    let len = stream.read_u8().await? as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let got = std::str::from_utf8(&buf)?;
    if got != expected {
        stream.write_u8(0x01).await?;
        stream.flush().await?;
        bail!("auth failed: bad token");
    }
    stream.write_u8(0x00).await?;
    stream.flush().await?;
    Ok(())
}

// ── yamux wrappers ──────────────────────────────────────────────────

/// Wrap a tokio AsyncRead+AsyncWrite stream into a yamux Connection.
///
/// The `.compat()` call bridges tokio's IO traits to futures' IO traits,
/// which yamux requires.
pub fn wrap_yamux<S>(stream: S, mode: Mode) -> Connection<Compat<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    Connection::new(stream.compat(), Config::default(), mode)
}

/// Drive the server side of a yamux connection.
///
/// Loops on `poll_next_inbound`, accepting new streams from the client (agent).
/// Each stream is handed to `handler` in a new tokio task. The handler receives
/// a `YamuxStream` implementing `futures::io::AsyncRead + AsyncWrite`.
pub async fn drive_server<S, F, Fut>(mut conn: Connection<Compat<S>>, handler: F)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: Fn(YamuxStream) -> Fut + Send + 'static + Clone,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let stream = poll_fn(|cx| conn.poll_next_inbound(cx)).await;
        match stream {
            Some(Ok(s)) => {
                let h = handler.clone();
                tokio::spawn(async move { h(s).await });
            }
            Some(Err(e)) => {
                error!("yamux inbound error: {e}");
                break;
            }
            None => {
                debug!("yamux connection closed");
                break;
            }
        }
    }
}

/// Drive the server side of a yamux connection with bidirectional stream support.
///
/// Like `drive_server`, but also accepts `ReverseForwardRequest`s to open
/// outbound yamux streams (server→agent). This enables the local machine to
/// initiate connections to the agent's network (e.g., to reach the remote
/// Docker daemon).
///
/// Uses the same single-`poll_fn` pattern as `drive_client` to avoid
/// cancel-safety issues with `poll_next_inbound`.
pub async fn drive_server_bidirectional<S, F, Fut>(
    mut conn: Connection<Compat<S>>,
    handler: F,
    mut reverse_rx: mpsc::Receiver<ReverseForwardRequest>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: Fn(YamuxStream) -> Fut + Send + 'static + Clone,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut pending_reverse: Option<ReverseForwardRequest> = None;

    poll_fn(|cx| {
        // 1. Drive yamux internals by draining all ready inbound streams.
        loop {
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(s))) => {
                    let h = handler.clone();
                    tokio::spawn(async move { h(s).await });
                }
                Poll::Ready(Some(Err(e))) => {
                    error!("yamux inbound error: {e}");
                    return Poll::Ready(());
                }
                Poll::Ready(None) => {
                    debug!("yamux connection closed");
                    return Poll::Ready(());
                }
                Poll::Pending => break,
            }
        }

        // 2. Try to complete any pending reverse forward request.
        if let Some(req) = pending_reverse.take() {
            match conn.poll_new_outbound(cx) {
                Poll::Ready(Ok(stream)) => {
                    let _ = req.respond.send(Ok(stream));
                }
                Poll::Ready(Err(e)) => {
                    let _ = req.respond.send(Err(e.into()));
                }
                Poll::Pending => {
                    pending_reverse = Some(req);
                }
            }
        }

        // 3. If no pending request, check channel for new ones.
        if pending_reverse.is_none() {
            match reverse_rx.poll_recv(cx) {
                Poll::Ready(Some(req)) => {
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(Ok(stream)) => {
                            let _ = req.respond.send(Ok(stream));
                        }
                        Poll::Ready(Err(e)) => {
                            let _ = req.respond.send(Err(e.into()));
                        }
                        Poll::Pending => {
                            pending_reverse = Some(req);
                        }
                    }
                }
                Poll::Ready(None) => {
                    // Reverse channel closed — continue serving inbound only
                    debug!("reverse forward channel closed");
                }
                Poll::Pending => {}
            }
        }

        Poll::Pending
    })
    .await;
}

/// A request from a SOCKS5 handler to the yamux driver to open an outbound stream.
///
/// The handler sends this via an mpsc channel, and the driver replies with the
/// opened `YamuxStream` (or error) via the oneshot.
pub struct StreamRequest {
    pub respond: oneshot::Sender<Result<YamuxStream>>,
}

/// Drive the client side of a yamux connection.
///
/// This is the trickiest part of the codebase. It must simultaneously:
/// 1. Poll `poll_next_inbound` to drive yamux's internal state machine (reading
///    frames, processing window updates, etc.)
/// 2. Accept `StreamRequest`s from SOCKS5 handlers via the mpsc channel
/// 3. Call `poll_new_outbound` to create new streams for those requests
///
/// All three must happen in **one `poll_fn`** because `poll_next_inbound` is not
/// cancel-safe — using `tokio::select!` between it and the channel would risk
/// dropping in-progress reads.
///
/// When an inbound stream starts with `0xFF`, it's a reverse-forwarded stream
/// from the server — the agent reads the target address and connects locally.
///
/// The flow:
/// - Always poll inbound first (drives yamux internals)
/// - Inbound streams with 0xFF prefix are reverse-forwarded (connect locally)
/// - Unexpected inbound streams without 0xFF are dropped
/// - If there's a pending outbound request, try to complete it via `poll_new_outbound`
/// - If no pending request, check the channel for new ones
pub async fn drive_client<S>(
    mut conn: Connection<Compat<S>>,
    mut rx: mpsc::Receiver<StreamRequest>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut pending_request: Option<oneshot::Sender<Result<YamuxStream>>> = None;

    poll_fn(|cx| {
        // 1. Drive yamux internals by draining all ready inbound streams.
        loop {
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => {
                    // Spawn a task to handle the inbound stream — it may be
                    // a reverse-forwarded connection from the server.
                    tokio::spawn(async move {
                        if let Err(e) = handle_inbound_stream(stream).await {
                            warn!("inbound stream error: {e:#}");
                        }
                    });
                }
                Poll::Ready(Some(Err(e))) => {
                    error!("yamux error: {e}");
                    return Poll::Ready(());
                }
                Poll::Ready(None) => {
                    debug!("yamux connection closed by peer");
                    return Poll::Ready(());
                }
                Poll::Pending => break,
            }
        }

        // 2. Try to complete any pending outbound stream request.
        if let Some(sender) = pending_request.take() {
            match conn.poll_new_outbound(cx) {
                Poll::Ready(Ok(stream)) => {
                    let _ = sender.send(Ok(stream));
                }
                Poll::Ready(Err(e)) => {
                    let _ = sender.send(Err(e.into()));
                }
                Poll::Pending => {
                    pending_request = Some(sender);
                }
            }
        }

        // 3. If no pending request, check channel for new ones.
        if pending_request.is_none() {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(req)) => {
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(Ok(stream)) => {
                            let _ = req.respond.send(Ok(stream));
                        }
                        Poll::Ready(Err(e)) => {
                            let _ = req.respond.send(Err(e.into()));
                        }
                        Poll::Pending => {
                            pending_request = Some(req.respond);
                        }
                    }
                }
                Poll::Ready(None) => {
                    debug!("stream request channel closed");
                    return Poll::Ready(());
                }
                Poll::Pending => {}
            }
        }

        Poll::Pending
    })
    .await;
}

/// Handle an inbound yamux stream on the agent (client) side.
///
/// If the first byte is `0xFF` (reverse prefix), this is a reverse-forwarded
/// connection from the server. Read the target address, connect locally, and
/// copy bytes bidirectionally. Otherwise, drop the stream.
async fn handle_inbound_stream(stream: YamuxStream) -> Result<()> {
    let mut stream_compat = stream.compat();

    // Peek at the first byte to determine stream type
    let prefix = stream_compat.read_u8().await?;

    if prefix != reverse::REVERSE_PREFIX {
        debug!("unexpected inbound stream (prefix {prefix:#x}), dropping");
        return Ok(());
    }

    // Read the target address (SOCKS5 format after the 0xFF prefix)
    let addr = Address::decode(&mut stream_compat).await?;
    info!("reverse forward: connecting to {addr}");

    let resolved = addr.resolve().await?;
    let mut target = TcpStream::connect(resolved).await?;
    info!("reverse forward: connected to {resolved}");

    tokio::io::copy_bidirectional(&mut stream_compat, &mut target).await?;

    Ok(())
}
