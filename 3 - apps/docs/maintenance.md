# System Maintenance

## Diagnostics

- **Script**: `cd ../scripts && ./diagnostics-unified.sh` from repo root (auto-discovers all servers).
- **Log**: Appends to **`logs/diagnostics.log`** and prints to terminal. Use **`--no-log`** for console-only.
- **Scope**: Memory/swap, DNS checks, Docker/Traefik/Sablier, Redis/Postgres, OOM log, Docker/SSH noise, UFW/ports.
- **Single-server**: SSH directly to `144.31.188.163` for box 3 only.

## Cleanup Schedules

### Unified cleanup (all servers)

- **Schedule**: Sunday 03:00 (cron, root)
- **Actions**: APT autoremove, journal vacuum (7d), btmp truncate, `/tmp` cleanup, Docker prune, n8n event logs + WAL checkpoint
- **Script**: `../scripts/cleanup-unified.sh` (SSHs into all 3 boxes)
- **Manual run**: `cd ../scripts && ./cleanup-unified.sh`

### Docker cleanup (ops3 only, weekly)

- **Schedule**: Sunday 04:00
- **Actions**: Stale images, build cache older than 7 days; protects `sablier.managed` containers
- **Script**: Deploy `docker-cleanup-safe.sh` to `/usr/local/bin/` on ops3

### Log cleanup (ops3 only, Sunday 03:30)

- **Actions**: journald vacuumed to 100MB; btmp and btmp.1 truncated
- **Script**: `ssh root@144.31.188.163 "journalctl --vacuum-size=100M && truncate -s 0 /var/log/btmp /var/log/btmp.1"`

## Swap

- **Path**: `/swapfile2` (1GB)
- **fstab**: `/swapfile2 swap swap sw 0 0`
- **Note**: Original `/swapfile` (512MB) was replaced; OOM during `swapoff` prevented in-place resize, so new file was added and fstab updated.

## SSH Security

- **Root**: Key-only auth (`PermitRootLogin prohibit-password`, `PasswordAuthentication no`)
- **Applied**: 2026-03-15

## Backups (Mac workstation)

- **Procedure**: [mac-workstation-backup/README.md](../../mac-workstation-backup/README.md) — `backup-ops3-ops4.sh` SSHs to this host (`144.31.188.163`) and ops4; stores under `BACKUP_ROOT/full-YYYY-MM-DD/ops3/` (Postgres dump, Redis RDB, `/data/projects` tarball).
- **Cron**: On the Mac, `caffeinate -i …/backup-ops3-ops4.sh` (see `mac-workstation-backup/crontab.example`); Full Disk Access for `/usr/sbin/cron` if using an external volume.
- **Secrets**: `POSTGRES_PASSWORD` and `REDIS_PASSWORD` in `mac-workstation-backup/.env` must match this repo’s `.env`.
