# SSH Key Access Setup Guide

## Overview
This guide explains how to enforce a **root-only, key-only** SSH posture on the VDS server (104.128.131.166). Passwords and IP whitelists are no longer part of the authentication flow, and the legacy multi-factor stack has been retired. Only trusted SSH keys bound to `root` are permitted.

## Prerequisites
- Root access to the VDS server
- A local SSH key that can authenticate as `root`
- Ability to copy the repository contents to `/root` on the server

## Initial Setup
1. Upload the repository to the server:
   ```bash
   scp -r scripts/ root@104.128.131.166:/root/
   scp -r docs/ root@104.128.131.166:/root/
   scp -r config/ root@104.128.131.166:/root/
   ```
2. Connect with your SSH key:
   ```bash
   ssh -i ~/.ssh/id_rsa root@104.128.131.166
   ```
3. Run the hardening script:
   ```bash
   sudo /root/scripts/setup/setup-ssh-key-policy.sh
   ```
   The script backs up existing configs, enforces the key-only snippet, resets UFW, and places the emergency helper script under `/root/scripts/utils/`.

## Daily Usage

### Normal Connection
Use the helper to keep SSH settings consistent:
```bash
./scripts/connect.sh
```

### Emergency Connection
If your primary key is unavailable, use the emergency key:
```bash
./scripts/connect.sh -e
# or
ssh -i /root/.ssh/emergency_key root@104.128.131.166
```

## Emergency Tools
- **Inspect status**: `./scripts/utils/ssh-emergency-access.sh -s`
- **Rotate emergency key**: `./scripts/utils/ssh-emergency-access.sh -r`
- **Restore backup**: `./scripts/utils/ssh-emergency-access.sh -b`

## Firewall Verification
The policy relies on UFW with strict defaults:
```bash
ufw status verbose
ufw allow 22/tcp comment "SSH key-only access"
ufw allow 80/tcp comment "HTTP"
ufw allow 443/tcp comment "HTTPS"
```
Accept only the ports listed above; other ports should remain blocked by default.

## Troubleshooting
- **Permission denied**: Confirm `/etc/ssh/sshd_config.d/99-key-only.conf` contains `AllowUsers root` and `PasswordAuthentication no`.
- **SSHD failing to restart**: `journalctl -u sshd -b | tail`
- **Emergency key missing**: Run `./scripts/utils/ssh-emergency-access.sh -r` to regenerate and authorize it.

## Maintenance
- Run `./scripts/test/test-ssh-key-policy.sh` after making changes.
- Rotate the emergency key quarterly: `./scripts/utils/ssh-emergency-access.sh -r`
- Review `/var/log/auth.log` for suspicious attempts.
- Keep `/root/scripts/utils/ssh-emergency-access.sh` executable.

## Monitoring
- Verify snippet content:
  ```bash
  cat /etc/ssh/sshd_config.d/99-key-only.conf
  ```
- Confirm `sshd -T` output lists the key-only settings.
- Check UFW and system logs regularly.

## Configuration Files
- `config/ssh-key-policy.conf` – documents the key policy parameters.
- `/etc/ssh/sshd_config.d/99-key-only.conf` – enforces `PermitRootLogin prohibit-password`, `AllowUsers root`, and `AuthenticationMethods publickey`.
- `/root/.ssh/emergency_key` – the offline emergency key pair.
- `/root/scripts/utils/ssh-emergency-access.sh` – helper for key operations.

## Support
If you need help:
1. Review the setup log: `ls /root/logs/setup-ssh-key-*.log`
2. Use emergency key access to recover control
3. Consult this guide again
