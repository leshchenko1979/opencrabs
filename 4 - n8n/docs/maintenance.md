# System maintenance — operations_4

## SSH

- Prefer **key-only** root after bootstrap: SSH directly to `2.27.120.75` using key auth.

## Firewall

- **UFW**: allow `22/tcp`, `80/tcp`, `443/tcp` (HTTP for ACME, HTTPS for n8n).

## Services

- **Caddy**: `systemctl status caddy`; config `/etc/caddy/Caddyfile`.
- **n8n**: `systemctl status n8n`; env `/etc/n8n.env`; logs `journalctl -u n8n -f`.
- **Picoclaw**: `systemctl --user status picoclaw picoclaw-webui`; health `curl http://127.0.0.1:18790/health`.

## Backups

- **Automated (Mac)**: [mac-workstation-backup/README.md](../../mac-workstation-backup/README.md) — daily `backup-ops3-ops4.sh` includes `/var/lib/n8n` and `/etc/n8n.env` under `full-YYYY-MM-DD/ops4/`.
- **Manual**: Tar `/var/lib/n8n` and copy `/etc/n8n.env` (exclude caches if you tarball by hand).

## Diagnostics

- From repo root: `cd ../scripts && ./diagnostics-unified.sh` (auto-discovers all boxes). Logs append to `logs/diagnostics.log` (gitignored).

## Disk cleanup (n8n host)

Safe reclaimers (stop `n8n` first if you want zero race): `npm cache clean --force` as root; remove `/root/.npm/_cacache` contents; clear `/var/lib/n8n/.cache/*` (n8n rebuilds UI assets). Then `apt-get clean`, `apt-get autoremove -y`, optional `journalctl --vacuum-size=…`. The unified cleanup script handles this automatically.
