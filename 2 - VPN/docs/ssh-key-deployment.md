# SSH Key Policy Deployment Guide

## Overview
This guide details how to deploy the root-only, key-only SSH policy across the VDS server (104.128.131.166). The deployment automates backups, SSH hardening, firewall reset, and emergency tooling.

## Prerequisites
- SSH access with an authorized key
- Root privileges on the VDS server
- `scp`/`rsync` available locally
- Ability to run `deploy-ssh-key-policy.sh` (the deployment helper script)

## Deployment Steps

### 1. Upload the repository assets
```bash
scp -r scripts/ root@104.128.131.166:/root/
scp -r docs/ root@104.128.131.166:/root/
scp -r config/ root@104.128.131.166:/root/
```

### 2. Run the deployment helper
```bash
./deploy-ssh-key-policy.sh
```
The helper script uploads the files, ensures the setup script is executable, and runs `/root/scripts/setup/setup-ssh-key-policy.sh`. It also prints the next steps for verifying the key-only policy.

### 3. Verify the configuration
- Check that `/etc/ssh/sshd_config.d/99-key-only.conf` exists and includes:
  ```bash
  PermitRootLogin prohibit-password
  PasswordAuthentication no
  AllowUsers root
  AuthenticationMethods publickey
  ```
- Confirm `ufw` is running with SSH, HTTP, and HTTPS allowed.
- Run `sshd -t` and `systemctl restart sshd`.

## Testing

### Normal SSH access
```bash
./scripts/connect.sh
```
Should log you in via your standard SSH key (no prompt for additional factors).

### Emergency access
```bash
./scripts/connect.sh -e
```
If that fails, connect directly:
```bash
ssh -i /root/.ssh/emergency_key root@104.128.131.166
```

### Firewall sanity
```bash
sudo ufw status verbose
```
Allowed ports must include 22, 80, and 443 while other ports remain blocked.

## Emergency Tools
- `./scripts/utils/ssh-emergency-access.sh -s` – inspect policy status and emergency key.
- `./scripts/utils/ssh-emergency-access.sh -r` – rotate the emergency key and refresh `authorized_keys`.
- `./scripts/utils/ssh-emergency-access.sh -b` – restore from the latest backup folder in `/root/backup-ssh-*`.

## Monitoring and Maintenance
- Review `/var/log/auth.log` and `journalctl -u sshd` regularly.
- Test emergency access monthly.
- Regenerate the emergency key and re-authorize it quarterly.
- Keep documentation current (`docs/ssh-key-setup.md`, `docs/ssh-key-recovery.md`) with any changes.

## Support
If you need help post-deployment:
1. Use emergency access to regain control.
2. Inspect `/root/logs/setup-ssh-key-*.log`.
3. Refer to `docs/ssh-key-implementation-summary.md` for the architecture.
