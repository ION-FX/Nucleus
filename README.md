# ⚛ Nucleus

A self-hosted game server management panel — a modern replacement for
[Pterodactyl](https://pterodactyl.io), built in Rust with first-class
**CurseForge modpack installs** and an **AI sysadmin** that diagnoses and
recovers crashed servers.

## Components

| Binary | Role |
|---|---|
| `nucleus-panel` | Web panel: users, nodes, eggs, servers. Server-rendered (Askama + vanilla JS, no Node). SQLite storage. |
| `nucleusd` | Node daemon: runs game servers as Docker containers, live console over WebSocket, file manager, tar.gz backups, modpack installer, AI agent. |

Browsers talk **only** to the panel; the panel proxies console/files/power to
daemons over HTTPS with a shared bearer token. Only one public port per box.

## Features

- **Servers as Docker containers** — CPU/memory limits via cgroups, port
  mappings, persistent `/data` volume bind-mounted per server.
- **Pterodactyl egg import** — upload egg `.json` files; docker images,
  `{{VAR}}` startup templates and variables are imported.
- **Modpack installer** (`nucleusd`) — upload through the panel:
  - CurseForge zip (`manifest.json`): extracts overrides, resolves + downloads
    every mod (works keyless via the public endpoint; set
    `curseforge_api_key` for reliability), reports per-mod results.
  - Modrinth `.mrpack`: extracts overrides + downloads from the index.
  - Generic server-pack zips: extracted with single-root flattening.
  - Progress is streamed into the server's install status / console.
- **Live console** — attach to process stdin/stdout; panel relays WebSocket;
  graceful stop honours the egg's stop command before falling back to
  `docker stop`.
- **File manager** — browse/edit/upload/download/delete inside `/data`
  (traversal-safe), plus backups: on-demand `tar.gz` create/list/download/delete.
- **SFTP** — embedded SSH/SFTP server (russh) on every node; per-server
  credentials (`srv.<id>` + password, shown/reset from the Files page), each
  session chrooted to that server's `/data` with symlink/`..` protection.
- **AI sysadmin** — OpenAI-compatible *and* Anthropic-compatible APIs. The
  agent gets tools (`get_status`, `get_recent_logs`, `power_action`,
  `send_console_command`, `list_files`, `read_file`), investigates crashes,
  restarts servers when allowed, and writes incident reports to
  `data/ai/<server>/incident-*.json`. With `auto_heal = true` it fires
  automatically on non-zero exits.

## Quick start (dev)

```bash
cargo build --release

# daemon
cp deploy/daemon.toml.example daemon.toml   # edit token!
./target/release/nucleusd --config daemon.toml

# panel (another shell)
cp deploy/panel.toml.example panel.toml     # point static_dir at crates/nucleus-panel/static
./target/release/nucleus-panel --config panel.toml
```

Open `http://localhost:8025` → create the admin account →
*Admin → Nodes* → add the daemon URL + token → *New server*.

## Production install

```bash
sudo ./deploy/install.sh
```

Installs binaries, generates a daemon token, enables
`nucleusd` + `nucleus-panel` systemd services. Requires Docker.

## Configuration

See `deploy/daemon.toml.example` (daemon incl. `[ai]` section) and
`deploy/panel.toml.example`.

For Anthropic-style providers:

```toml
[ai]
provider = "anthropic"
model = "claude-sonnet-4-5"
api_key = "env:ANTHROPIC_API_KEY"
```

Any OpenAI-compatible endpoint works too (LM Studio, vLLM, OpenRouter…):

```toml
[ai]
provider = "openai"
base_url = "http://localhost:1234/v1"
```

## Security notes (v1)

- Daemon API requires the shared bearer token on every route except `/health`.
- File paths are jailed to each server's data dir (canonicalised, `..` and
  symlink escapes rejected).
- Passwords use argon2id; sessions are HttpOnly cookies.
- No CSRF protection yet — don't expose the panel publicly without adding it.
- Containers currently run as root inside; drop `user:` mapping is planned.

## Roadmap

- Scheduled tasks/restarts, subusers & granular ACLs, 2FA
- Direct browser→daemon console mode (skip panel proxy)
- Egg install scripts executed in a sidecar container
