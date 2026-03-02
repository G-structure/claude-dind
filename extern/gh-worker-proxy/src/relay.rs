//! TCP relay for NAT traversal.
//!
//! When both the server (your machine) and the agent (GHA runner) are behind
//! NAT, neither can accept inbound connections from the other. The relay solves
//! this: both sides connect *outbound* to the relay, which pairs them and does
//! dumb byte forwarding.
//!
//! # How it works
//!
//! ```text
//! [serve] ──outbound──▶ [relay] ◀──outbound── [agent]
//!                          │
//!                   copy_bidirectional
//! ```
//!
//! 1. The relay binds a TCP listener and waits for connections.
//! 2. Each client sends a handshake: `[role, token_len, token_bytes]`.
//!    - Role `0x01` = server, `0x02` = agent.
//!    - The relay validates the token and replies `0x00` (accepted) or `0x01`
//!      (rejected).
//! 3. Accepted connections are routed to one of two mpsc channels (server or
//!    agent) based on their role.
//! 4. A **pairer task** receives from both channels. When it has one server and
//!    one agent, it sends `0x02` (paired) to both, then spawns a
//!    `copy_bidirectional` task to forward bytes between them.
//!
//! # Security model
//!
//! The relay is intentionally simple — it doesn't terminate TLS, inspect
//! traffic, or understand yamux. The tunnel's TLS passes through opaquely,
//! so the relay never sees plaintext. The relay token is a separate credential
//! from the tunnel auth token; it only controls who can use the relay for
//! pairing.
//!
//! # Limitations
//!
//! - One server pairs with one agent (FIFO). No session multiplexing.
//! - The relay must be publicly reachable (a VPS, cloud instance, etc.).
//! - For a serverless alternative, see `relay-worker/` (Cloudflare Worker +
//!   Durable Object with WebSocket pairing).

use anyhow::Result;
use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// CLI arguments for `gwp relay`.
#[derive(Args)]
pub struct RelayArgs {
    /// Port to listen on
    #[arg(long, default_value = "9443")]
    port: u16,

    /// Token that clients must present
    #[arg(long)]
    relay_token: String,
}

/// Entry point for the `gwp relay` subcommand.
///
/// Sets up the pairer task and accept loop. The pairer and acceptor communicate
/// via two mpsc channels — one for server connections, one for agent connections.
/// This design means the accept loop never blocks waiting for a pair; it just
/// routes authenticated connections into the right channel.
pub async fn run(args: RelayArgs) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", args.port)).await?;
    info!("relay listening on 0.0.0.0:{}", args.port);

    // Channels for pairing: server side and agent side
    let (server_tx, mut server_rx) = mpsc::channel::<TcpStream>(8);
    let (agent_tx, mut agent_rx) = mpsc::channel::<TcpStream>(8);

    // Pairer task: match one server with one agent.
    // Uses `tokio::join!` to wait for both sides simultaneously. When both
    // arrive, it sends the 0x02 "paired" signal and spawns a forwarding task.
    tokio::spawn(async move {
        loop {
            let (server, agent) = tokio::join!(server_rx.recv(), agent_rx.recv());
            match (server, agent) {
                (Some(mut s), Some(mut a)) => {
                    info!("pairing server and agent");
                    // Notify both sides that pairing is complete
                    let s_notify = s.write_u8(0x02);
                    let a_notify = a.write_u8(0x02);
                    if let Err(e) = tokio::try_join!(s_notify, a_notify) {
                        warn!("failed to notify pair: {e}");
                        continue;
                    }

                    tokio::spawn(async move {
                        if let Err(e) = tokio::io::copy_bidirectional(&mut s, &mut a).await {
                            info!("tunnel ended: {e}");
                        }
                    });
                }
                _ => {
                    info!("pairer channel closed");
                    break;
                }
            }
        }
    });

    // Accept loop — read handshake, validate token, route to correct channel.
    loop {
        let (tcp, peer) = listener.accept().await?;
        info!("relay connection from {peer}");

        let server_tx = server_tx.clone();
        let agent_tx = agent_tx.clone();
        let expected_token = args.relay_token.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_relay_client(tcp, &expected_token, server_tx, agent_tx).await {
                warn!("relay client {peer}: {e:#}");
            }
        });
    }
}

/// Handle a single relay client connection.
///
/// Reads the handshake (`[role, token_len, token_bytes]`), validates the token,
/// and routes the connection into the appropriate mpsc channel for the pairer
/// task to pick up.
///
/// The handshake is intentionally minimal — just enough to authenticate and
/// identify roles. Everything after the `0x02` "paired" response is opaque
/// tunnel bytes (TLS → auth → yamux).
async fn handle_relay_client(
    mut tcp: TcpStream,
    expected_token: &str,
    server_tx: mpsc::Sender<TcpStream>,
    agent_tx: mpsc::Sender<TcpStream>,
) -> Result<()> {
    // Handshake: [role_byte, token_len, token_bytes]
    let role = tcp.read_u8().await?;
    let token_len = tcp.read_u8().await? as usize;
    let mut token_buf = vec![0u8; token_len];
    tcp.read_exact(&mut token_buf).await?;
    let token = std::str::from_utf8(&token_buf)?;

    if token != expected_token {
        tcp.write_u8(0x01).await?; // reject
        anyhow::bail!("bad relay token");
    }
    tcp.write_u8(0x00).await?; // accept
    tcp.flush().await?;

    match role {
        0x01 => {
            info!("server registered, waiting for agent...");
            server_tx
                .send(tcp)
                .await
                .map_err(|_| anyhow::anyhow!("pairer gone"))?;
        }
        0x02 => {
            info!("agent registered, waiting for server...");
            agent_tx
                .send(tcp)
                .await
                .map_err(|_| anyhow::anyhow!("pairer gone"))?;
        }
        _ => anyhow::bail!("unknown role {role:#x}"),
    }

    Ok(())
}
