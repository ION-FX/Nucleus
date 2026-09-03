Three-workstream hardening cycle for Nucleus, in this order. Deferred (not picked this round): post-creation env editor + the four survey bug fixes (invite URL localhost, dead Defaults page, non-admin "+ New server" button, perm guards on startup/settings saves).

## Workstream 1 — Backup safety (daemon + panel UI)

**Retention/pruning**
- Add optional `backup_retention: u32` and `backup_quiesce: Option<bool>` to the server spec in `crates/nucleus-core/src/dto.rs` (serde defaults — old registries keep loading). Spec is persisted in `servers.json` automatically.
- `crates/nucleusd/src/backups.rs`: after a successful backup create, if retention is set, sort backups by timestamp filename and delete the oldest beyond the limit; log pruned names to the server console (`[nucleus] pruned old backup <name>`).
- Extend daemon `POST /servers/{id}/config` (`docker.rs`/`routes.rs` config patch DTO) to accept the two new fields.

**Quiesce (safe world save)**
- Before tarring, when the server is running and `backup_quiesce` resolves true (default: auto-detect Minecraft-family via `server.properties` presence, same detection the port-sync uses), send `save-all flush` via existing `docker::send_command`, wait up to 10s for a completion line in recent logs (fallback: fixed 8s), then archive. Off → current behavior.

**Panel side**
- `backups.html`: small "Backup policy" form (Keep last N = 0/∞, quiesce checkbox/auto) posting through a new panel route in `routes/proxy.rs` that PATCHes the daemon config route.

## Workstream 2 — Daemon security (TLS + request signing)

**TLS on the daemon API**
- `crates/nucleusd/src/config.rs`: new `[tls]` block (enabled, cert_path, key_path) — all optional, old configs unchanged.
- `crates/nucleusd/src/main.rs`: serve via `axum-server` + rustls when TLS enabled (add `axum-server` + `rustls`/pem deps); plain bind otherwise. WS upgrade works over it unchanged.
- Panel `src/daemon.rs`: node URL scheme drives https; new per-node fields `tls_insecure` (accept self-signed) and optional `tls_ca_path` wired into the shared reqwest client builder + `tokio-tungstenite` WS connect (custom connector). DB migration for the node columns.

**Per-request authentication (replaces bearer-only)**
- Daemon middleware in `routes.rs`: verify `X-Nucleus-Timestamp` + `X-Nucleus-Signature` = HMAC-SHA256(token, `{timestamp}.{method}.{path}.{sha256(body)}`), ±60s clock window, constant-time compare. Plain bearer still accepted while `[auth] allow_bearer = true` (default, transition; set false to enforce signing only).
- Panel `DaemonClient`: centralize all HTTP calls through one signing helper (needs body bytes up front — Json payloads serialized once); sign WS upgrade request headers too.
- Update `deploy/daemon.toml.example` + a short deploy/SECURITY note with cert generation (openssl one-liner) and how the panel config references it.

## Workstream 3 — Public API v1 expansion (panel `routes/api.rs`)

Extend the Bearer `nuc_` API with the same perm model (admin-gated where noted):
- Files: `GET /servers/{id}/files?path=` list, `GET .../files/content?path=` read, `PUT` write (JSON text/base64), `POST .../files/mkdir|delete|rename`
- Backups: `GET` list, `POST` create, `GET .../backups/{bid}/download` (streamed through panel), `DELETE`, `POST .../restore`
- Schedules: `GET`/`POST`/`PUT`/`DELETE` + `POST .../run`
- Servers (admin keys only): `POST /servers` create, `DELETE /servers/{id}` (with `purge_data` flag), `PATCH .../config` (same DTO as daemon, minus env — env editing stays deferred)
- Consistent JSON error shape `{"error": "..."}` + 401/403/404 semantics; document endpoints in `deploy/API.md`.

## Verification & delivery
- `cargo test --release --workspace` green each workstream.
- Live on the test instance (daemon 127.0.0.1:18033, panel 18025): retention cycle (create 3 backups with retention=2 → oldest pruned, console line present), quiesce on running NGK17 (console shows `save-all` + "Saved the game" before backup), TLS daemon + panel green https + `--insecure` curl, signed-request e2e with bearer disabled, then re-enabled default, and each new API endpoint exercised via curl with a `nuc_` key.
- One commit per workstream, push to main. Friend-facing install.sh unaffected (new config fields all optional).