//! Exit node — the server side of the reverse tunnel.
//!
//! This is the half that runs on **your local machine**. It accepts a TLS
//! connection from the agent (directly, through a TCP relay, or through a
//! Cloudflare Worker WebSocket relay), validates the auth token, sets up yamux,
//! and then for each inbound yamux stream:
//!
//! 1. Reads the SOCKS5-encoded target address from the stream header
//! 2. Resolves DNS locally (the `socks5h://` semantic — "h" means the proxy
//!    resolves hostnames, not the client)
//! 3. Dials the target over plain TCP
//! 4. Copies bytes bidirectionally between the yamux stream and the TCP socket
//!
//! # Connection modes
//!
//! The server supports three connectivity modes:
//!
//! - **Direct** (no `--relay`): Binds a TCP listener and accepts inbound
//!   connections. Requires the server to be reachable from the internet (port
//!   forwarding, public IP, etc.). Each connection is spawned into its own task,
//!   so multiple agents can connect simultaneously.
//!
//! - **TCP relay** (`--relay HOST:PORT`): Connects outbound to `gwp relay`,
//!   identifies as role `0x01` (server), and waits for the relay to pair it with
//!   an agent. Only one session at a time.
//!
//! - **WebSocket relay** (`--relay wss://...`): Connects outbound to a
//!   Cloudflare Worker relay via WebSocket, which pairs it with an agent. The WS
//!   connection is bridged to a `DuplexStream` that looks like a normal
//!   `AsyncRead + AsyncWrite` stream to the rest of the code.
//!
//! # Generics
//!
//! `handle_connection_io` is generic over `IO: AsyncRead + AsyncWrite + Unpin`.
//! This lets us pass in a raw `TcpStream` (direct or TCP relay), a `DuplexStream`
//! (WebSocket relay), or any other transport without code duplication. TLS
//! accept, auth, yamux setup, and stream handling are all transport-agnostic.

use anyhow::Result;
use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{info, warn};
use yamux::{Mode, Stream as YamuxStream};

use crate::reverse::ReverseForwardRequest;
use crate::socks5::Address;
use crate::{tls, tunnel, ws};

/// CLI arguments for `gwp serve`.
#[derive(Args)]
pub struct ServeArgs {
    /// Port to listen on for agent connections
    #[arg(long, default_value = "8443")]
    port: u16,

    /// Pre-set auth token (random hex generated if omitted)
    #[arg(long)]
    token: Option<String>,

    /// Connect to relay instead of listening directly.
    /// TCP: HOST:PORT, WebSocket: wss://relay.example.workers.dev
    #[arg(long)]
    relay: Option<String>,

    /// Token for relay authentication / session pairing
    #[arg(long)]
    relay_token: Option<String>,
}

/// Entry point for the `gwp serve` subcommand.
///
/// Generates a fresh TLS identity (self-signed cert + private key), prints the
/// SHA-256 fingerprint and auth token for the user to pass to the agent, then
/// enters one of the three connection modes based on CLI arguments.
pub async fn run(args: ServeArgs) -> Result<()> {
    let identity = tls::generate_identity()?;
    let token = args.token.unwrap_or_else(|| {
        use rand::Rng;
        let bytes: [u8; 16] = rand::thread_rng().gen();
        hex::encode(bytes)
    });

    info!("fingerprint: {}", identity.fingerprint);
    info!("token: {token}");

    let acceptor = TlsAcceptor::from(identity.tls_config);

    if let Some(relay_addr) = args.relay {
        let relay_token = args
            .relay_token
            .ok_or_else(|| anyhow::anyhow!("--relay-token required with --relay"))?;

        if relay_addr.starts_with("ws://") || relay_addr.starts_with("wss://") {
            let url = format!("{relay_addr}/connect?token={relay_token}&role=server");
            info!("connecting to WS relay: {relay_addr}");
            let stream = ws::connect(&url).await?;
            info!("WS relay connected, waiting for TLS handshake...");
            handle_connection_io(acceptor, &token, stream).await?;
        } else {
            info!("connecting to TCP relay at {relay_addr}");
            let mut tcp = TcpStream::connect(&relay_addr).await?;
            relay_handshake(&mut tcp, 0x01, &relay_token).await?;
            info!("relay paired, waiting for TLS handshake...");
            handle_connection_io(acceptor, &token, tcp).await?;
        }
    } else {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port)).await?;
        info!("listening on 0.0.0.0:{}", args.port);

        loop {
            let (tcp, peer) = listener.accept().await?;
            info!("accepted connection from {peer}");
            let acceptor = acceptor.clone();
            let token = token.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection_io(acceptor, &token, tcp).await {
                    warn!("connection from {peer} failed: {e:#}");
                }
            });
        }
    }

    Ok(())
}

/// Perform the TCP relay handshake (server side).
///
/// Protocol: send `[role, token_len, token_bytes]`, expect `0x00` (accepted),
/// then wait for `0x02` (paired with an agent). After this returns, the TCP
/// stream carries raw tunnel bytes — TLS, auth, and yamux happen on top.
async fn relay_handshake(tcp: &mut TcpStream, role: u8, token: &str) -> Result<()> {
    let tb = token.as_bytes();
    let mut buf = Vec::with_capacity(2 + tb.len());
    buf.push(role);
    buf.push(tb.len() as u8);
    buf.extend_from_slice(tb);
    tcp.write_all(&buf).await?;
    tcp.flush().await?;

    let ack = tcp.read_u8().await?;
    if ack != 0x00 {
        anyhow::bail!("relay rejected (code {ack:#x})");
    }
    info!("relay accepted, waiting for pair...");
    let pair = tcp.read_u8().await?;
    if pair != 0x02 {
        anyhow::bail!("unexpected relay byte {pair:#x}");
    }
    Ok(())
}

/// Handle an agent connection over any transport.
///
/// This is the core connection lifecycle on the server side:
/// 1. TLS accept (server side of handshake — presents our self-signed cert)
/// 2. Auth handshake (validate the agent's token)
/// 3. Wrap the TLS stream in yamux (`Mode::Server`)
/// 4. Drive the yamux connection, spawning `handle_stream` for each inbound stream
///
/// If `reverse_rx` is provided, also handles `ReverseForwardRequest`s to open
/// outbound yamux streams to the agent (for reverse port forwarding).
///
/// Generic over `IO` so it works with `TcpStream` (direct/relay), `DuplexStream`
/// (WebSocket relay), or any other `AsyncRead + AsyncWrite` transport.
pub async fn handle_connection_io<IO>(acceptor: TlsAcceptor, token: &str, io: IO) -> Result<()>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    handle_connection_io_with_reverse(acceptor, token, io, None).await
}

/// Like `handle_connection_io` but with optional reverse port forwarding support.
pub async fn handle_connection_io_with_reverse<IO>(
    acceptor: TlsAcceptor,
    token: &str,
    io: IO,
    reverse_rx: Option<mpsc::Receiver<ReverseForwardRequest>>,
) -> Result<()>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut tls = acceptor.accept(io).await?;
    info!("TLS established");

    tunnel::auth_server(&mut tls, token).await?;
    info!("auth OK");

    let conn = tunnel::wrap_yamux(tls, Mode::Server);

    let stream_handler = |stream| async move {
        if let Err(e) = handle_stream(stream).await {
            warn!("stream error: {e:#}");
        }
    };

    if let Some(rx) = reverse_rx {
        tunnel::drive_server_bidirectional(conn, stream_handler, rx).await;
    } else {
        tunnel::drive_server(conn, stream_handler).await;
    }

    Ok(())
}

/// Handle a single yamux stream — one proxied connection.
///
/// Each yamux stream starts with a SOCKS5-format address header (written by the
/// agent's `handle_socks5`). We decode it, resolve DNS locally, connect to the
/// target over TCP, and then copy bytes in both directions until either side
/// closes.
///
/// The `FuturesAsyncReadCompatExt` converts the yamux stream (which implements
/// `futures::io::AsyncRead/Write`) back into a tokio-compatible stream so we
/// can use `tokio::io::copy_bidirectional`.
async fn handle_stream(stream: YamuxStream) -> Result<()> {
    let mut stream_compat = stream.compat();
    let addr = Address::decode(&mut stream_compat).await?;
    info!("connect request: {addr}");

    let resolved = addr.resolve().await?;
    let mut target = TcpStream::connect(resolved).await?;
    info!("connected to {resolved}");

    tokio::io::copy_bidirectional(&mut stream_compat, &mut target).await?;

    Ok(())
}
