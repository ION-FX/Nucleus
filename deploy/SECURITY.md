# Securing a Nucleus deployment

## Node↔panel transport

The daemon API (`nucleusd`, port 8033 by default) is the highest-value surface:
its token can drive every game server on the box. Two layers protect it.

### 1. TLS on the daemon API

In the daemon's `daemon.toml`:

```toml
[tls]
enabled = true
cert_path = "/etc/nucleus/daemon-cert.pem"
key_path = "/etc/nucleus/daemon-key.pem"
```

Generate a keypair (self-signed example):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout /etc/nucleus/daemon-key.pem -out /etc/nucleus/daemon-cert.pem \
  -days 825 -nodes -subj "/CN=your-node-hostname" \
  -addext "subjectAltName=DNS:your-node-hostname,IP:203.0.113.10"
```

In the panel's **Admin → Nodes**, register/edit the node with an
`https://` URL and pick one of:

- **CA bundle path** — copy `daemon-cert.pem` to the panel host and put its
  path in the node's CA field. Full verification; recommended.
- **Accept self-signed** — certificate is not verified (still encrypted).
  Use only where the network path to the node is otherwise trusted.

The console WebSocket relay uses the same trust settings automatically.

### 2. Request signing (anti-replay / anti-token-theft)

The panel signs every request to a node:

```
X-Nucleus-Timestamp: <unix seconds>
X-Nucleus-Signature: hex(HMAC-SHA256(token, "{timestamp}.{METHOD}.{path}?{query}"))
```

The daemon rejects signatures older than `[auth] max_skew_secs` (default 60).
For nodes reachable from the internet, disable the bearer fallback:

```toml
[auth]
allow_bearer = false   # default true, for localhost / trusted LANs
```

Note: signatures cover method + path + query and freshness, **not** the body.
Body integrity and confidentiality come from TLS — enable both layers for
internet-facing nodes.

## Other surfaces

- **Panel** (`nucleus-panel`): serve behind your own reverse proxy with TLS
  (nginx/caddy); sessions are HttpOnly cookies.
- **SFTP** (port 2022): always SSH-encrypted (russh, Ed25519 host key);
  credentials are per-server and resettable from the server's Files page.
- **Docker**: the daemon controls Docker with root equivalents — run it on a
  dedicated box/user; do not expose port 8033 publicly without both layers
  above.
