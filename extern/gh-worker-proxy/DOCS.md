# gwp — Technical Documentation

## Motivation

CI runners (GitHub Actions, etc.) run on ephemeral cloud VMs. Sometimes you need them to make network requests that appear to come from your machine — hitting internal services, accessing geo-restricted APIs, or testing against resources that only resolve from your network. A VPN is overkill. Exposing ports is fragile. gwp creates a temporary, authenticated tunnel that routes the runner's traffic through you.

## Design decisions

### Why a single Rust binary?

The predecessor was a shell script orchestrating chisel (Go tunnel) and cloudflared (Cloudflare Tunnel). Three moving parts, three failure modes, version skew between tools, and a 50MB+ download on each CI run. A single Rust binary eliminates all of that — one `cargo build`, one artifact, deterministic behavior.

### Why SOCKS5 and not HTTP CONNECT?

SOCKS5 operates at the transport layer and can proxy any TCP connection, not just HTTP. It also supports passing domain names to the proxy (`socks5h://` in curl), so DNS resolution happens on the exit node. This is critical — an internal hostname that resolves on your network may not resolve from the runner's Azure/AWS DNS.

### Why self-signed TLS with fingerprint pinning?

Traditional certificate validation requires either:
- A CA-signed certificate (Let's Encrypt, etc.) — needs a domain, ACME challenge, renewal
- A custom CA whose root cert is distributed to clients — key management overhead

Neither makes sense for an ephemeral tunnel. Instead: the server generates a fresh ECDSA P-256 keypair on each run, computes SHA-256 of the DER-encoded certificate, and prints the fingerprint. The agent is given this fingerprint via CLI argument and verifies it using a custom `rustls::client::danger::ServerCertVerifier`. This is equivalent to SSH's known_hosts model — trust on first use with an out-of-band fingerprint check.

TLS handshake signature verification still happens through `rustls::crypto::verify_tls12_signature` / `verify_tls13_signature` — the server must prove it holds the private key matching the pinned certificate. We only skip X.509 chain validation (issuer, expiry, subject name), not cryptographic proof of key possession.

### Why ring and not aws-lc-rs?

`rustls` defaults to the `aws-lc-rs` crypto backend, which requires a C compiler and CMake to build. GitHub Actions runners have these, but it adds build time and complexity. The `ring` backend compiles from Rust + assembly with no C toolchain, keeping the CI build simple. We explicitly select it via `rustls = { features = ["ring"] }` and `builder_with_provider(ring::default_provider())`.

### Why yamux?

We need to multiplex many logical connections over one TLS tunnel. Each SOCKS5 CONNECT request becomes one bidirectional stream. yamux (Yet Another Multiplexer) is a simple, well-tested stream multiplexer used by libp2p and others. It handles flow control (per-stream windows), stream creation/teardown, and keepalives.

### Why not tokio::select! with yamux?

yamux's `Connection::poll_next_inbound` is **not cancel-safe**. It maintains internal state between polls — partially read frame headers, buffered data, pending window updates. If you use it inside `tokio::select!` and the other branch fires first, the future is dropped, and that internal state is lost. Data corruption or hangs follow.

The fix: a single `poll_fn` closure that drives **all** yamux operations:
1. Always poll `poll_next_inbound` first (drives yamux internals — frame processing, window updates, keepalives)
2. If there's a pending outbound stream request, poll `poll_new_outbound`
3. If no pending request, check the mpsc channel for new requests

No `select!`, no dropped futures, no lost state. This is the trickiest part of the codebase — see `tunnel::drive_client` for the implementation.

### Why a Cloudflare Worker relay?

Both your machine and the GHA runner are typically behind NAT — neither can accept inbound connections from the other. We need a relay that both sides connect to outbound. Options:

1. **VPS running `gwp relay`** — Works, but requires maintaining a server
2. **Cloudflare Worker + Durable Object** — Serverless, globally distributed, free tier covers typical usage

The Worker routes WebSocket connections to a Durable Object keyed by the relay token. The DO pairs a "server" and "agent" WebSocket, then forwards binary frames between them. The tunnel's TLS passes through opaquely — the Worker never sees plaintext.

The DO uses the Hibernatable WebSocket API (`state.acceptWebSocket()`) so it can be evicted from memory between messages. A `pendingMessages` buffer handles the race where one side connects and starts the TLS handshake before the other side arrives.

### Why native-tls for tokio-tungstenite?

`tokio-tungstenite` needs a TLS backend for `wss://` URLs. Its `rustls` integration pulls in `aws-lc-rs` by default (the C compiler problem again). Using the `native-tls` feature instead leverages the platform's TLS stack (SecureTransport on macOS, OpenSSL on Linux). This is fine because the WebSocket connects to Cloudflare's edge (a real CA-signed certificate), not to our self-signed tunnel cert.

### Why DuplexStream for the WebSocket bridge?

The tunnel stack expects `AsyncRead + AsyncWrite` (a byte stream). WebSocket is message-based (discrete frames). The bridge creates a `tokio::io::DuplexStream` — an in-memory pipe — and spawns two pump tasks:

- **WS → duplex**: Reads Binary frames, writes raw bytes into the pipe
- **duplex → WS**: Reads bytes from the pipe, wraps as Binary frames

The "local" end of the duplex is returned to the caller. From the tunnel code's perspective, it's a normal byte stream. The 256KB duplex buffer absorbs bursts; the 64KB read buffer matches typical TCP segment sizes.

## Protocol details

### TLS layer

- Server generates a self-signed ECDSA P-256 cert via `rcgen::generate_simple_self_signed(["localhost"])`
- SHA-256 of the DER-encoded certificate is the fingerprint (lowercase hex)
- Agent uses a custom `ServerCertVerifier` that compares `sha256(presented_cert_der) == expected_fingerprint`
- SNI is always "localhost" (doesn't matter — we don't validate subject names)
- TLS 1.2 and 1.3 are supported via `with_safe_default_protocol_versions()`

### Auth handshake (over TLS, before yamux)

```
client → server: [token_len: u8] [token_bytes: [u8; token_len]]
server → client: [0x00] (OK) or [0x01] (rejected)
```

Runs over the raw TLS stream before yamux starts. The explicit `flush()` after each write is critical — `tokio-rustls` buffers like `BufWriter`, so without flush the bytes sit in an internal buffer and the other side hangs waiting.

### yamux multiplexing

After auth, the TLS stream is wrapped with `yamux::Connection`:
- Server side: `Mode::Server`, accepts inbound streams via `poll_next_inbound`
- Client side: `Mode::Client`, opens outbound streams via `poll_new_outbound`

yamux uses `futures::io` traits (not tokio's). The `tokio_util::compat` module bridges them:
- `TokioAsyncReadCompatExt::compat()` wraps a tokio stream for yamux (`Connection::new`)
- `FuturesAsyncReadCompatExt::compat()` wraps a yamux stream back for tokio (`copy_bidirectional`)

### Stream protocol (over yamux)

The agent opens a yamux stream per SOCKS5 CONNECT request and writes a connect header:
```
[atyp: u8] [addr: variable] [port: u16 big-endian]
```

This is identical to SOCKS5 address encoding (ATYP + ADDR + PORT). We reuse the same wire format to avoid inventing another protocol. The server reads this header, resolves DNS, connects to the target, and does bidirectional byte copying.

### SOCKS5 (agent side)

Minimal implementation — just enough for `curl --socks5-hostname`:
- **Auth**: NOAUTH (0x00) only — no username/password
- **Commands**: CONNECT (0x01) only — no BIND or UDP ASSOCIATE
- **Address types**: IPv4 (0x01), domain name (0x03), IPv6 (0x04)

The `socks5h://` semantic means hostnames are passed through to the server for DNS resolution. The agent never resolves DNS — it passes the raw domain name over the yamux stream.

### TCP relay protocol

Both serve and agent connect outbound to the relay. Handshake:
```
client → relay: [role: u8] [token_len: u8] [token_bytes: [u8; token_len]]
relay → client: [0x00] (accepted)
relay → client: [0x02] (paired with counterpart)
```

- Role `0x01` = server, `0x02` = agent
- After both sides are paired, the relay does `copy_bidirectional`
- The tunnel's TLS passes through opaquely — the relay never sees plaintext

### WebSocket relay protocol

Both sides connect to `wss://<worker>/connect?token=<T>&role=<R>`. The Worker routes to a Durable Object keyed by the token. The DO accepts WebSockets, tags them by role, and forwards Binary frames between the two. Message buffering handles the race where one side connects before the other.

## Module overview

| Module | Lines | Purpose |
|--------|-------|---------|
| `main.rs` | ~60 | CLI (clap derive), tokio runtime, tracing init |
| `tls.rs` | ~160 | Cert generation, fingerprint verifier, TLS client/server configs |
| `socks5.rs` | ~180 | SOCKS5 NOAUTH+CONNECT, Address encode/decode/resolve |
| `tunnel.rs` | ~220 | Auth handshake, yamux wrap/drive (server + client) |
| `serve.rs` | ~140 | Exit node: TLS accept, DNS resolve, stream handler, relay modes |
| `agent.rs` | ~160 | SOCKS5 proxy: TLS connect, yamux streams, relay modes |
| `relay.rs` | ~110 | TCP relay: pair server+agent, byte forwarding |
| `ws.rs` | ~60 | WebSocket ↔ DuplexStream bridge |
| `relay-worker/src/index.js` | ~75 | Cloudflare Durable Object relay |

## Key implementation notes

1. **yamux cancel safety**: `poll_next_inbound` is NOT cancel-safe. The client driver uses a single `poll_fn` that drives inbound polling, outbound stream creation, and channel receives — no `tokio::select!`.

2. **TLS flush after auth**: `tokio-rustls` buffers writes internally. Every auth write must be followed by an explicit `flush()` or the other side hangs.

3. **`futures::io` vs `tokio::io`**: yamux uses futures traits; tokio-rustls uses tokio traits. `tokio_util::compat` bridges them in both directions. This is the most confusing part for anyone reading the code for the first time.

4. **DNS resolution**: Happens on the server side only. The agent passes hostnames through unresolved. This is the `socks5h://` semantic — the "h" means the proxy resolves hostnames.

5. **Generics for transport**: Both `serve::handle_connection_io` and `agent::run_tunnel` are generic over `IO: AsyncRead + AsyncWrite`. This lets them accept `TcpStream`, `DuplexStream`, or any other transport without code duplication.

6. **Relay token vs tunnel token**: Two separate credentials. The relay token controls who can use the relay for pairing. The tunnel token authenticates the agent to the server over TLS. The relay never sees the tunnel token (it's inside the TLS envelope).

## Environment variables

- `RUST_LOG` — Controls tracing output. Default: `gwp=info`. Example: `RUST_LOG=gwp=debug` for verbose output.

## Dependencies

| Crate | Why |
|-------|-----|
| `clap` | CLI argument parsing with derive macros |
| `tokio` | Async runtime (full features: net, io, sync, macros) |
| `tokio-rustls` | TLS over tokio streams |
| `rustls` | TLS implementation (with `ring` backend, not `aws-lc-rs`) |
| `rcgen` | Self-signed certificate generation |
| `yamux` | Stream multiplexing over a single connection |
| `tokio-util` | `compat` module bridging futures::io ↔ tokio::io |
| `futures` | `AsyncWriteExt` for yamux streams, `SinkExt`/`StreamExt` for WS |
| `tokio-tungstenite` | WebSocket client (with `native-tls` feature) |
| `tracing` / `tracing-subscriber` | Structured logging with env-filter |
| `rand` | Random token generation |
| `sha2` / `hex` | SHA-256 fingerprint computation |
| `anyhow` | Error handling with context |
