/**
 * Cloudflare Worker + Durable Object relay for gwp.
 *
 * This is the serverless alternative to `gwp relay`. Instead of running a VPS
 * with `gwp relay --port 9443`, you deploy this Worker to Cloudflare. Both
 * `gwp serve` and `gwp agent` connect to it via WebSocket, and the Durable
 * Object pairs them and forwards binary frames.
 *
 * ## How it works
 *
 * 1. Both serve and agent connect to `wss://<worker>/connect?token=<T>&role=<R>`
 * 2. The Worker routes the request to a Durable Object named by the token — so
 *    both sides with the same token land in the same DO instance.
 * 3. The DO accepts the WebSocket, tags it with the role ("server" or "agent").
 * 4. When a message arrives on one WebSocket, the DO forwards it to the other.
 * 5. The tunnel's TLS passes through opaquely — the DO just forwards binary
 *    frames without inspecting them.
 *
 * ## Message buffering
 *
 * If the first side (say, serve) connects and starts the TLS handshake before
 * the agent connects, its outgoing TLS bytes would be lost. The `pendingMessages`
 * array buffers messages from the first connector. When the second connector
 * arrives, the buffered messages are flushed to it before any new messages flow.
 *
 * ## Hibernatable WebSocket API
 *
 * We use `state.acceptWebSocket()` instead of manually handling the WebSocket in
 * `fetch()`. This is the Hibernatable WebSocket API — the DO can be evicted from
 * memory between messages and automatically wakes up when a message arrives.
 * This matters for billing (you only pay for active time) and for sessions that
 * may have idle periods.
 *
 * ## Deployment
 *
 *   cd relay-worker
 *   npx wrangler deploy
 *
 * The Worker URL (e.g. `wss://gwp-relay.<your-account>.workers.dev`) is passed
 * to `gwp serve --relay` and `gwp agent --relay`.
 */

/**
 * Worker fetch handler — routes /connect requests to the Durable Object.
 *
 * The token query parameter doubles as the DO name, so both serve and agent
 * with the same token are routed to the same DO instance. The role parameter
 * ("server" or "agent") is passed through to the DO for tagging.
 */
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== "/connect") {
      return new Response("gwp relay");
    }

    const token = url.searchParams.get("token");
    const role = url.searchParams.get("role");
    if (!token || !["server", "agent"].includes(role)) {
      return new Response("bad params", { status: 400 });
    }

    // Use the token as the DO name — both sides with the same token
    // are routed to the same Durable Object instance.
    const id = env.RELAY.idFromName(token);
    const stub = env.RELAY.get(id);
    return stub.fetch(request);
  },
};

/**
 * Durable Object that pairs two WebSocket connections and forwards frames.
 *
 * Lifecycle:
 * 1. First connector: WebSocket accepted, tagged with role, stored via
 *    `state.acceptWebSocket()`. Messages are buffered in `pendingMessages`.
 * 2. Second connector: WebSocket accepted, buffered messages flushed to it.
 * 3. Steady state: `webSocketMessage` forwards frames between the two sockets.
 * 4. Close: When either side disconnects, the other is closed too.
 *
 * Maximum 2 WebSockets per DO instance (enforced by the `session full` check).
 */
export class Relay {
  constructor(state) {
    this.state = state;
    /** @type {Array<ArrayBuffer|string>} Messages buffered before second peer connects */
    this.pendingMessages = [];
  }

  /**
   * Handle an incoming WebSocket upgrade request.
   *
   * Creates a WebSocketPair, accepts the server-side socket into the DO's
   * hibernatable storage (tagged with the role), and returns the client-side
   * socket as the HTTP response. If the other peer is already connected,
   * flushes any buffered messages to the new arrival.
   */
  async fetch(request) {
    const url = new URL(request.url);
    const role = url.searchParams.get("role");

    const existing = this.state.getWebSockets();
    if (existing.length >= 2) {
      return new Response("session full", { status: 409 });
    }

    const pair = new WebSocketPair();
    const [client, ws] = Object.values(pair);
    this.state.acceptWebSocket(ws, [role]);

    // If the other side is already connected, flush buffered messages
    // to the newcomer so the TLS handshake bytes aren't lost.
    if (existing.length === 1) {
      for (const msg of this.pendingMessages) {
        ws.send(msg);
      }
      this.pendingMessages = [];
    }

    return new Response(null, { status: 101, webSocket: client });
  }

  /**
   * Forward a message from one peer to the other.
   *
   * If only one peer is connected, buffer the message for later delivery.
   * This handles the race where serve starts the TLS handshake before the
   * agent has connected to the relay.
   */
  async webSocketMessage(ws, message) {
    const others = this.state.getWebSockets().filter((s) => s !== ws);
    if (others.length > 0) {
      for (const other of others) {
        other.send(message);
      }
    } else {
      this.pendingMessages.push(message);
    }
  }

  /**
   * When one peer disconnects, close the other side too.
   * Propagates the close code and reason for clean shutdown.
   */
  async webSocketClose(ws, code, reason) {
    for (const sock of this.state.getWebSockets()) {
      if (sock !== ws) {
        try {
          sock.close(code, reason || "");
        } catch {}
      }
    }
  }

  /**
   * On WebSocket error, close the errored socket.
   */
  async webSocketError(ws, error) {
    ws.close(1011, "error");
  }
}
