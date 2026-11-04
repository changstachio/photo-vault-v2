#!/bin/sh
# Install the release binary and enable the daemon at boot. Run as root from the repo root.
set -eu
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
install -m 0755 target/release/vault /usr/local/bin/vault
mkdir -p /etc/photovault
[ -e /etc/photovault/config.toml ] || /usr/local/bin/vault init /etc/photovault/config.toml
id photovault >/dev/null 2>&1 || useradd --system --create-home --shell /usr/sbin/nologin photovault
# The web UI saves settings by writing this file, so the service user owns it.
chown photovault: /etc/photovault /etc/photovault/config.toml
install -m 0644 install/photovault.service /etc/systemd/system/photovault.service
systemctl daemon-reload
echo "Edit /etc/photovault/config.toml and set User= in the unit if needed, then:"
echo "  vault --config /etc/photovault/config.toml check"
echo "  systemctl enable --now photovault"
