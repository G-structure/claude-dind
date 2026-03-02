//! gwp — In-process reverse SOCKS5 tunnel.
//!
//! A single binary with three subcommands that together create a tunnel allowing
//! a remote machine (e.g. a GitHub Actions runner) to route its network traffic
//! through your local machine. The "reverse" in "reverse SOCKS5" means the SOCKS5
//! proxy lives on the remote side, but the traffic exits on the local side.
//!
//! # Subcommands
//!
//! - **serve**: Runs on your local machine. Accepts a TLS connection (directly or
//!   via relay), then serves as the exit node — receiving connect requests over
//!   yamux streams, resolving DNS, dialing targets, and copying bytes.
//!
//! - **agent**: Runs on the remote machine (GHA runner). Opens a SOCKS5 listener
//!   on localhost. For each SOCKS5 CONNECT request, it opens a yamux stream through
//!   the TLS tunnel to the server, which does the actual network call.
//!
//! - **relay**: Optional middleman for NAT traversal. Both serve and agent connect
//!   outbound to the relay, which pairs them and does dumb byte forwarding. Two
//!   implementations exist: a TCP relay (`gwp relay`) and a Cloudflare Worker +
//!   Durable Object relay (WebSocket-based, in `relay-worker/`).

use anyhow::Result;
use clap::Parser;

mod agent;
mod http_connect;
mod relay;
mod reverse;
mod serve;
mod socks5;
mod tls;
mod tunnel;
mod ws;

#[derive(Parser)]
#[command(name = "gwp", about = "In-process reverse SOCKS5 tunnel")]
enum Cli {
    /// Run the exit node (your machine)
    Serve(serve::ServeArgs),
    /// Run the SOCKS5 proxy (GHA runner)
    Agent(agent::AgentArgs),
    /// Run a public relay for NAT traversal
    Relay(relay::RelayArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gwp=info".parse().unwrap()),
        )
        .init();

    match Cli::parse() {
        Cli::Serve(args) => serve::run(args).await,
        Cli::Agent(args) => agent::run(args).await,
        Cli::Relay(args) => relay::run(args).await,
    }
}
