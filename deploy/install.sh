#!/usr/bin/env bash
# Nucleus installer: builds both binaries and sets up systemd services.
# Run as root on a machine with Docker installed.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run as root: sudo $0" >&2
  exit 1
fi

SRC_DIR="$(cd "$(dirname "$0")/.." && pwd)"
echo "==> Building Nucleus (release) from $SRC_DIR"
( cd "$SRC_DIR" && cargo build --release )

echo "==> Installing binaries"
install -m 0755 "$SRC_DIR/target/release/nucleusd" /usr/local/bin/nucleusd
install -m 0755 "$SRC_DIR/target/release/nucleus-panel" /usr/local/bin/nucleus-panel

echo "==> Creating users and directories"
id nucleus &>/dev/null || useradd --system --home /var/lib/nucleus --shell /usr/sbin/nologin nucleus
mkdir -p /etc/nucleus /var/lib/nucleus
chown nucleus:nucleus /var/lib/nucleus

echo "==> Installing static assets"
mkdir -p /usr/share/nucleus/panel
cp -r "$SRC_DIR/crates/nucleus-panel/static" /usr/share/nucleus/panel/

if [[ ! -f /etc/nucleus/daemon.toml ]]; then
  TOKEN="$(head -c32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  sed "s/CHANGE-ME-long-random-string/$TOKEN/" "$SRC_DIR/deploy/daemon.toml.example" > /etc/nucleus/daemon.toml
  echo "    Generated daemon token: $TOKEN   (put this in the panel when adding the node)"
else
  echo "==> Keeping existing /etc/nucleus/daemon.toml"
fi

[[ -f /etc/nucleus/panel.toml ]] || install -m 0644 "$SRC_DIR/deploy/panel.toml.example" /etc/nucleus/panel.toml

echo "==> Installing systemd units"
install -m 0644 "$SRC_DIR/deploy/nucleusd.service" /etc/systemd/system/
install -m 0644 "$SRC_DIR/deploy/nucleus-panel.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now nucleusd nucleus-panel

cat <<'DONE'

Nucleus installed.

  Panel : http://<host>:8025   (first visit -> create the admin account)
  Daemon: :8033                (register in Panel -> Admin -> Nodes with the token above)

Logs: journalctl -u nucleusd -u nucleus-panel -f
DONE
