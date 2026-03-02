//! SOCKS5 proxy — the agent side of the reverse tunnel.
//!
//! This is the half that runs on the **remote machine** (e.g. a GitHub Actions
//! runner). It does two things:
//!
//! 1. **Establishes the tunnel**: Connects to the server (directly, through a TCP
//!    relay, or through a Cloudflare Worker WebSocket relay), performs TLS
//!    handshake with fingerprint verification, authenticates with a token, and
//!    sets up yamux in client mode.
//!
//! 2. **Runs a SOCKS5 proxy**: Binds a local listener (default `127.0.0.1:1080`)
//!    that accepts standard SOCKS5 CONNECT requests. For each request, it asks
//!    the yamux driver for a new outbound stream, writes the target address as a
//!    SOCKS5-format header, and copies bytes bidirectionally.
//!
//! # Why "reverse"
//!
//! In a normal SOCKS5 proxy, the proxy server connects to the target. Here the
//! SOCKS5 listener is on the *remote* side, but the actual network calls happen
//! on the *server* (your local machine). The agent sends connect requests
//! through the tunnel; the server resolves DNS and dials targets from your
//! network. This is what makes it "reverse" — traffic exits through the
//! server's network, not the agent's.
//!
//! # Stream request flow
//!
//! ```text
//! SOCKS5 client ──► handle_socks5 ──► mpsc::Sender<StreamRequest>
//!                                            │
//!                                            ▼
//!                                     drive_client (poll_fn)
//!                                            │
//!                                            ▼  poll_new_outbound
//!                                     yamux Connection
//!                                            │
//!                                            ▼  oneshot reply
//! SOCKS5 client ◄── handle_socks5 ◄── YamuxStream
//! ```
//!
//! The mpsc channel decouples SOCKS5 handlers from the yamux driver. Each
//! handler sends a `StreamRequest` containing a oneshot sender, the driver
//! opens a yamux stream and replies through the oneshot. This design avoids
//! sharing the yamux `Connection` across tasks (it's `!Sync`).
//!
//! # Connection modes
//!
//! Same three modes as `serve`:
//! - **Direct** (`--server HOST:PORT`): TCP connect to a reachable server
//! - **TCP relay** (`--relay HOST:PORT`): Connect to `gwp relay`, role `0x02`
//! - **WebSocket relay** (`--relay wss://...`): Connect to Cloudflare Worker relay

use anyhow::Result;
use clap::Args;
use futures::AsyncWriteExt as _;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{info, warn};
use yamux::Mode;

use crate::{http_connect, socks5, tls, tunnel, ws};

/// CLI arguments for `gwp agent`.
#[derive(Args)]
pub struct AgentArgs {
    /// Server address (direct mode)
    #[arg(long)]
    server: Option<String>,

    /// Relay address (relay mode).
    /// TCP: HOST:PORT, WebSocket: wss://relay.example.workers.dev
    #[arg(long)]
    relay: Option<String>,

    /// Relay auth token / session pairing token
    #[arg(long)]
    relay_token: Option<String>,

    /// Server TLS certificate fingerprint (SHA256 hex)
    #[arg(long)]
    fingerprint: String,

    /// Server auth token
    #[arg(long)]
    token: String,

    /// SOCKS5 listen address
    #[arg(long, default_value = "127.0.0.1:1080")]
    listen: String,

    /// HTTP CONNECT proxy listen address (for Node.js HTTPS_PROXY)
    #[arg(long, default_value = "127.0.0.1:1081")]
    http_listen: String,
}

/// Entry point for the `gwp agent` subcommand.
///
/// Builds a fingerprint-pinning TLS connector, then enters one of the three
/// connection modes. All modes converge on `run_tunnel` which handles TLS,
/// auth, yamux, and the SOCKS5 listener.
pub async fn run(args: AgentArgs) -> Result<()> {
    let connector = tls::make_connector(args.fingerprint.clone());
    let sni = ServerName::try_from("localhost")?;

    if let Some(relay_addr) = &args.relay {
        let relay_token = args
            .relay_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--relay-token required with --relay"))?;

        if relay_addr.starts_with("ws://") || relay_addr.starts_with("wss://") {
            let url = format!("{relay_addr}/connect?token={relay_token}&role=agent");
            info!("connecting to WS relay: {relay_addr}");
            let stream = ws::connect(&url).await?;
            info!("WS relay connected");
            run_tunnel(connector, sni, stream, &args.token, &args.listen, &args.http_listen).await?;
        } else {
            info!("connecting to TCP relay at {relay_addr}");
            let mut tcp = TcpStream::connect(relay_addr).await?;
            relay_handshake(&mut tcp, 0x02, relay_token).await?;
            info!("relay paired");
            run_tunnel(connector, sni, tcp, &args.token, &args.listen, &args.http_listen).await?;
        }
    } else if let Some(server_addr) = &args.server {
        info!("connecting to server at {server_addr}");
        let tcp = TcpStream::connect(server_addr).await?;
        run_tunnel(connector, sni, tcp, &args.token, &args.listen, &args.http_listen).await?;
    } else {
        anyhow::bail!("either --server or --relay is required");
    }

    Ok(())
}

/// Set up the tunnel and run the SOCKS5 listener.
///
/// This is the core lifecycle on the agent side:
/// 1. TLS connect with fingerprint verification (SNI is always "localhost" since
///    we don't use CA validation — the fingerprint is the trust anchor)
/// 2. Auth handshake (send token, expect OK)
/// 3. Wrap the TLS stream in yamux (`Mode::Client`)
/// 4. Spawn the yamux driver task (`drive_client`) with an mpsc channel for
///    stream requests
/// 5. Bind the SOCKS5 listener, accept connections, spawn `handle_socks5` per
///    client
///
/// Generic over `IO` for the same reason as `serve::handle_connection_io` —
/// works with any `AsyncRead + AsyncWrite` transport.
pub async fn run_tunnel<IO>(
    connector: tokio_rustls::TlsConnector,
    sni: ServerName<'static>,
    io: IO,
    token: &str,
    listen: &str,
    http_listen: &str,
) -> Result<()>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut tls = connector.connect(sni, io).await?;
    info!("TLS established (fingerprint verified)");

    tunnel::auth_client(&mut tls, token).await?;
    info!("auth OK");

    let conn = tunnel::wrap_yamux(tls, Mode::Client);

    let (tx, rx) = mpsc::channel::<tunnel::StreamRequest>(32);

    tokio::spawn(async move {
        tunnel::drive_client(conn, rx).await;
        info!("yamux driver exited");
    });

    // Spawn HTTP CONNECT proxy alongside SOCKS5
    let http_tx = tx.clone();
    let http_addr = http_listen.to_string();
    tokio::spawn(async move {
        if let Err(e) = http_connect::run_http_connect(&http_addr, http_tx).await {
            warn!("HTTP CONNECT proxy error: {e:#}");
        }
    });

    let listener = TcpListener::bind(listen).await?;
    info!("SOCKS5 listening on {listen}");

    loop {
        let (tcp, peer) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_socks5(tcp, tx).await {
                warn!("SOCKS5 {peer}: {e:#}");
            }
        });
    }
}

/// Perform the TCP relay handshake (agent side, role `0x02`).
///
/// Identical protocol to `serve::relay_handshake` but with role byte `0x02`
/// instead of `0x01`. See `relay.rs` for the relay-side implementation.
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

/// Handle one SOCKS5 client connection.
///
/// Flow:
/// 1. SOCKS5 handshake — negotiate NOAUTH, parse CONNECT request, extract target
///    address. The SOCKS5 success reply is sent immediately (before we've actually
///    connected) because the real connection is made by the server, and we don't
///    want to block on the round-trip.
/// 2. Request a yamux stream — send a `StreamRequest` through the mpsc channel
///    to the yamux driver, which calls `poll_new_outbound` and replies with the
///    stream via oneshot.
/// 3. Write connect header — the target address in SOCKS5 wire format (ATYP +
///    ADDR + PORT). The server's `handle_stream` reads this to know where to
///    connect. We reuse the SOCKS5 encoding to avoid inventing another format.
/// 4. Bidirectional copy — `tokio::io::copy_bidirectional` between the local TCP
///    socket (SOCKS5 client) and the yamux stream (tunneled to the server).
///
/// The `futures::AsyncWriteExt` import is needed because yamux streams implement
/// `futures::io::AsyncWrite` (not tokio's). We use `.compat()` to bridge back
/// to tokio for `copy_bidirectional`.
async fn handle_socks5(
    mut tcp: TcpStream,
    tx: mpsc::Sender<tunnel::StreamRequest>,
) -> Result<()> {
    let addr = socks5::handshake(&mut tcp).await?;
    info!("SOCKS5 CONNECT {addr}");

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(tunnel::StreamRequest { respond: resp_tx })
        .await
        .map_err(|_| anyhow::anyhow!("yamux driver gone"))?;

    let mut yamux_stream = resp_rx
        .await
        .map_err(|_| anyhow::anyhow!("yamux driver dropped request"))?
        .map_err(|e| anyhow::anyhow!("yamux open stream failed: {e:#}"))?;

    let encoded = addr.encode();
    yamux_stream.write_all(&encoded).await?;
    yamux_stream.flush().await?;

    let mut yamux_compat = yamux_stream.compat();
    tokio::io::copy_bidirectional(&mut tcp, &mut yamux_compat).await?;

    Ok(())
}
