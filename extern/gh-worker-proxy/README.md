# gwp — Reverse SOCKS5 tunnel

A single Rust binary that creates a reverse SOCKS5 tunnel, allowing a remote machine (e.g. a GitHub Actions runner) to route its network traffic through your local machine. The "reverse" means the SOCKS5 proxy lives on the remote side, but traffic exits on the local side.

## The idea

You want a CI runner to make network requests **as if they came from your machine**. Maybe you need to hit internal services, test geo-dependent APIs, or access resources that are only available from your network. Instead of exposing your network or setting up a VPN, gwp creates a tunnel: the runner proxies its traffic through you.

```
[GHA runner]                                     [Your machine]
                                                  192.168.x.x
curl ──▶ SOCKS5 :1080 ──yamux──TLS──relay──▶ dial target ──▶ Internet
                                                  ▲
                                          traffic exits here
```

The runner sees your IP. DNS resolves from your network. No inbound ports needed on either side.

## Architecture

```
[agent on GHA runner]                      [serve on your Mac]
SOCKS5 client (curl etc)                   target hosts
  │                                             ▲
  ▼                                             │
SOCKS5 listener :1080                      dial target, copy bytes
  │                                             ▲
  ▼                                             │
yamux stream (open_stream) ──tunnel──▶  yamux stream (next_stream)
  │                                             ▲
  ▼                                             │
yamux Connection                           yamux Connection
  │                                             ▲
  ▼                                             │
tokio-rustls TlsStream    ◀────────▶    tokio-rustls TlsStream
  │                                             │
  ▼                                             ▼
  └───── both connect outbound to relay ────────┘
```

Three subcommands, one binary:

- **`gwp serve`** — Runs on your machine. The exit node. Accepts yamux streams, resolves DNS, connects to targets, copies bytes.
- **`gwp agent`** — Runs on the remote machine. Opens a local SOCKS5 listener. Each SOCKS5 CONNECT request becomes a yamux stream through the tunnel.
- **`gwp relay`** — Optional middleman for NAT traversal. Both sides connect outbound; the relay pairs them and forwards bytes.

## Quick start

### With Cloudflare Worker relay (recommended)

Deploy the relay (one-time):
```sh
cd relay-worker
npm install
npx wrangler deploy
# Note the URL: https://gwp-relay.<account>.workers.dev
```

On your machine:
```sh
cargo build --release
./target/release/gwp serve \
  --relay wss://gwp-relay.<account>.workers.dev \
  --relay-token mysecret
# Prints: fingerprint: <FP>
# Prints: token: <TOKEN>
```

On the runner (or locally to test):
```sh
./target/release/gwp agent \
  --relay wss://gwp-relay.<account>.workers.dev \
  --relay-token mysecret \
  --fingerprint <FP> \
  --token <TOKEN>
```

Test:
```sh
curl --socks5-hostname 127.0.0.1:1080 http://ifconfig.me
# Should show your machine's IP, not the runner's
```

### Direct mode (server reachable from internet)

On your machine:
```sh
gwp serve --port 8443
```

On the runner:
```sh
gwp agent --server <YOUR_IP>:8443 --fingerprint <FP> --token <TOKEN>
```

### TCP relay mode

On a public server:
```sh
gwp relay --port 9443 --relay-token <RTOKEN>
```

On your machine:
```sh
gwp serve --relay <RELAY_IP>:9443 --relay-token <RTOKEN>
```

On the runner:
```sh
gwp agent --relay <RELAY_IP>:9443 --relay-token <RTOKEN> \
  --fingerprint <FP> --token <TOKEN>
```

## Security

- **Self-signed TLS with fingerprint pinning**: The server generates a fresh ECDSA P-256 keypair on each run. The agent verifies the SHA-256 fingerprint of the DER-encoded certificate — no CA trust store, no Let's Encrypt, no system certs.
- **Token authentication**: A random hex token is exchanged over TLS before yamux starts. Even if someone discovers the relay URL and server fingerprint, they can't use the tunnel without the token.
- **Relay opacity**: The relay (TCP or Worker) is a dumb byte/frame forwarder. Tunnel TLS passes through opaquely — the relay never sees plaintext.

## Building

```sh
cargo build --release
# Binary: target/release/gwp (~3.2MB stripped)
```

Uses the `ring` crypto backend for rustls — no C compiler or CMake required. Builds cleanly on GitHub Actions runners without extra toolchain setup.

## GHA workflow

See `.github/workflows/proxy-test.yml`. Triggered via `workflow_dispatch` with inputs: `fingerprint`, `token`, `relay_addr`, `relay_token`. The workflow builds the binary, starts the agent, and verifies the proxy by comparing the runner's direct IP against the proxied IP.

## Project structure

```
gh-worker-proxy/
├── Cargo.toml                          # Binary = "gwp", all Rust deps
├── src/
│   ├── main.rs                         # CLI (clap), tokio runtime, tracing
│   ├── tls.rs                          # Cert gen, fingerprint verifier, TLS configs
│   ├── socks5.rs                       # SOCKS5 NOAUTH+CONNECT, Address type
│   ├── tunnel.rs                       # Auth handshake, yamux drivers
│   ├── serve.rs                        # Exit node subcommand
│   ├── agent.rs                        # SOCKS5 proxy subcommand
│   ├── relay.rs                        # TCP relay subcommand
│   └── ws.rs                           # WebSocket ↔ byte-stream bridge
├── relay-worker/
│   ├── wrangler.toml                   # Cloudflare Worker config
│   ├── package.json
│   └── src/index.js                    # Durable Object relay
├── .github/workflows/proxy-test.yml    # GHA workflow
└── DOCS.md                             # Technical documentation
```
