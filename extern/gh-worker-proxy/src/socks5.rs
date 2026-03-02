//! Minimal SOCKS5 protocol implementation (NOAUTH + CONNECT only).
//!
//! This module handles just enough of SOCKS5 to let `curl --socks5-hostname` work.
//! The implementation supports:
//! - Authentication: NOAUTH (0x00) only — no username/password
//! - Commands: CONNECT (0x01) only — no BIND or UDP ASSOCIATE
//! - Address types: IPv4 (0x01), domain name (0x03), IPv6 (0x04)
//!
//! # Why SOCKS5 and not HTTP CONNECT
//!
//! SOCKS5 operates at the transport layer and can proxy any TCP connection, not just
//! HTTP. It also supports passing domain names to the proxy (`socks5h://` in curl),
//! so DNS resolution happens on the exit node — important for accessing resources
//! that resolve differently from the runner vs. your local network.
//!
//! # Address reuse
//!
//! The [`Address`] type and its `encode()`/`decode()` methods use the exact same
//! wire format as SOCKS5 addresses (ATYP + ADDR + PORT). We reuse this encoding
//! as the "connect header" sent over yamux streams, avoiding a separate protocol.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::lookup_host;

/// A target address extracted from a SOCKS5 CONNECT request.
///
/// Variants match the SOCKS5 ATYP field:
/// - `Ipv4`: ATYP 0x01 — 4-byte IPv4 + 2-byte port
/// - `Domain`: ATYP 0x03 — length-prefixed hostname + 2-byte port
/// - `Ipv6`: ATYP 0x04 — 16-byte IPv6 + 2-byte port
#[derive(Debug, Clone)]
pub enum Address {
    Ipv4([u8; 4], u16),
    Domain(String, u16),
    Ipv6([u8; 16], u16),
}

impl Address {
    /// Encode as SOCKS5 wire format: ATYP + ADDR + PORT (big-endian).
    ///
    /// This encoding is reused as the yamux stream "connect header" — the agent
    /// writes it at the start of each yamux stream so the server knows where to
    /// connect.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            Address::Ipv4(ip, port) => {
                buf.push(0x01);
                buf.extend_from_slice(ip);
                buf.extend_from_slice(&port.to_be_bytes());
            }
            Address::Domain(name, port) => {
                buf.push(0x03);
                buf.push(name.len() as u8);
                buf.extend_from_slice(name.as_bytes());
                buf.extend_from_slice(&port.to_be_bytes());
            }
            Address::Ipv6(ip, port) => {
                buf.push(0x04);
                buf.extend_from_slice(ip);
                buf.extend_from_slice(&port.to_be_bytes());
            }
        }
        buf
    }

    /// Decode from SOCKS5 wire format (async reader).
    ///
    /// Used both for SOCKS5 handshake parsing (agent side) and for reading
    /// the connect header from yamux streams (server side).
    pub async fn decode<R: AsyncRead + Unpin>(r: &mut R) -> Result<Self> {
        let atyp = r.read_u8().await?;
        match atyp {
            0x01 => {
                let mut ip = [0u8; 4];
                r.read_exact(&mut ip).await?;
                let port = r.read_u16().await?;
                Ok(Address::Ipv4(ip, port))
            }
            0x03 => {
                let len = r.read_u8().await? as usize;
                let mut name = vec![0u8; len];
                r.read_exact(&mut name).await?;
                let port = r.read_u16().await?;
                Ok(Address::Domain(String::from_utf8(name)?, port))
            }
            0x04 => {
                let mut ip = [0u8; 16];
                r.read_exact(&mut ip).await?;
                let port = r.read_u16().await?;
                Ok(Address::Ipv6(ip, port))
            }
            _ => bail!("unsupported SOCKS5 ATYP: {atyp:#x}"),
        }
    }

    /// Resolve to a concrete `SocketAddr`. For domain names, performs async
    /// DNS lookup via `tokio::net::lookup_host`.
    ///
    /// This is called on the **server side only** — the agent passes domain names
    /// through unresolved, and the server resolves them against its own DNS.
    /// This is the `socks5h://` semantic (the "h" means the proxy resolves hostnames).
    pub async fn resolve(&self) -> Result<SocketAddr> {
        match self {
            Address::Ipv4(ip, port) => Ok(SocketAddr::new(Ipv4Addr::from(*ip).into(), *port)),
            Address::Ipv6(ip, port) => Ok(SocketAddr::new(Ipv6Addr::from(*ip).into(), *port)),
            Address::Domain(name, port) => {
                let addr_str = format!("{name}:{port}");
                let addr = lookup_host(&addr_str)
                    .await?
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("DNS lookup failed for {addr_str}"))?;
                Ok(addr)
            }
        }
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Ipv4(ip, port) => write!(f, "{}:{port}", Ipv4Addr::from(*ip)),
            Address::Domain(name, port) => write!(f, "{name}:{port}"),
            Address::Ipv6(ip, port) => write!(f, "[{}]:{port}", Ipv6Addr::from(*ip)),
        }
    }
}

/// Perform the SOCKS5 NOAUTH+CONNECT handshake on an already-connected stream.
///
/// Protocol flow:
/// 1. Client sends greeting: `[0x05, nmethods, methods...]`
/// 2. We reply with NOAUTH: `[0x05, 0x00]`
/// 3. Client sends connect request: `[0x05, 0x01, 0x00, ATYP, ADDR, PORT]`
/// 4. We reply with success: `[0x05, 0x00, 0x00, 0x01, 0,0,0,0, 0,0]`
///
/// Returns the target [`Address`] the client wants to connect to.
/// The actual connection is made elsewhere (over the yamux tunnel to the server).
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Address> {
    // --- Greeting ---
    let ver = stream.read_u8().await?;
    if ver != 0x05 {
        bail!("not SOCKS5: version {ver:#x}");
    }
    let nmethods = stream.read_u8().await? as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;
    // Reply: NOAUTH selected
    stream.write_all(&[0x05, 0x00]).await?;
    stream.flush().await?;

    // --- Connect request ---
    let ver = stream.read_u8().await?;
    if ver != 0x05 {
        bail!("SOCKS5 connect: bad version {ver:#x}");
    }
    let cmd = stream.read_u8().await?;
    if cmd != 0x01 {
        // Reply: command not supported (0x07)
        stream
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await?;
        bail!("unsupported SOCKS5 command: {cmd:#x}");
    }
    let _rsv = stream.read_u8().await?; // reserved byte, always 0x00
    let addr = Address::decode(stream).await?;

    // Reply: success, bound address 0.0.0.0:0 (we don't know the real bound addr)
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    stream.flush().await?;

    Ok(addr)
}
