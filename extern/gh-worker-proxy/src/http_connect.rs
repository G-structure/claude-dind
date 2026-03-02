//! HTTP CONNECT proxy that chains through the yamux tunnel.
//!
//! Node.js (Claude Code) doesn't support SOCKS5 via `HTTPS_PROXY`. This module
//! provides a minimal HTTP CONNECT proxy that accepts connections on a local port,
//! parses the `CONNECT host:port` request, and tunnels the connection through the
//! existing yamux tunnel to the server (which resolves DNS and dials the target).

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{info, warn};

use crate::socks5::Address;
use crate::tunnel::StreamRequest;

/// Run the HTTP CONNECT proxy listener.
///
/// Binds to `listen_addr` and for each incoming connection:
/// 1. Reads the HTTP CONNECT request line and headers
/// 2. Parses the target host:port
/// 3. Opens a yamux stream via the shared `StreamRequest` channel
/// 4. Replies with `HTTP/1.1 200 Connection established`
/// 5. Copies bytes bidirectionally
pub async fn run_http_connect(
    listen_addr: &str,
    tx: mpsc::Sender<StreamRequest>,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!("HTTP CONNECT proxy listening on {listen_addr}");

    loop {
        let (tcp, peer) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connect(tcp, tx).await {
                warn!("HTTP CONNECT {peer}: {e:#}");
            }
        });
    }
}

/// Handle a single HTTP CONNECT request.
async fn handle_connect(
    tcp: TcpStream,
    tx: mpsc::Sender<StreamRequest>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(tcp);
    let mut buf_reader = BufReader::new(reader);

    // Read the request line: "CONNECT host:port HTTP/1.1\r\n"
    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line).await?;

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "CONNECT" {
        writer
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await?;
        anyhow::bail!("not a CONNECT request: {request_line}");
    }

    let target = parts[1];
    let (host, port) = parse_host_port(target)?;

    // Read and discard remaining headers until empty line
    loop {
        let mut line = String::new();
        buf_reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }

    info!("HTTP CONNECT {host}:{port}");

    // Request a yamux stream from the tunnel driver
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    tx.send(StreamRequest { respond: resp_tx })
        .await
        .map_err(|_| anyhow::anyhow!("yamux driver gone"))?;

    let mut yamux_stream = resp_rx
        .await
        .map_err(|_| anyhow::anyhow!("yamux driver dropped request"))?
        .map_err(|e| anyhow::anyhow!("yamux open stream failed: {e:#}"))?;

    // Write SOCKS5-format address header to the yamux stream
    let addr = Address::Domain(host, port);
    let encoded = addr.encode();
    use futures::AsyncWriteExt as _;
    yamux_stream.write_all(&encoded).await?;
    yamux_stream.flush().await?;

    // Send 200 Connection established
    writer
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;

    // Reunite reader and writer back into a single stream for copy_bidirectional
    let mut tcp_combined = buf_reader.into_inner().unsplit(writer);
    let mut yamux_compat = yamux_stream.compat();
    tokio::io::copy_bidirectional(&mut tcp_combined, &mut yamux_compat).await?;

    Ok(())
}

/// Parse "host:port" into (host, port).
fn parse_host_port(target: &str) -> Result<(String, u16)> {
    // Handle [ipv6]:port
    if let Some(bracket_end) = target.find(']') {
        let host = target[1..bracket_end].to_string();
        let port_str = &target[bracket_end + 2..]; // skip ]:
        let port: u16 = port_str.parse()?;
        return Ok((host, port));
    }

    // Handle host:port
    let colon = target
        .rfind(':')
        .ok_or_else(|| anyhow::anyhow!("no port in CONNECT target: {target}"))?;
    let host = target[..colon].to_string();
    let port: u16 = target[colon + 1..].parse()?;
    Ok((host, port))
}
