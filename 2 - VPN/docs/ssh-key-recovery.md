# SSH Key Policy Recovery Procedures

## Overview
This document covers recovery flows when the root-only SSH key policy locks you out of the VDS server. The goal is to provide fast recovery using the emergency key, backup scripts, and documented commands.

## Emergency Access Methods

### 1. Emergency SSH Key (Primary recovery)
Use this key when your standard key is unavailable:
```bash
ssh -i /root/.ssh/emergency_key root@104.128.131.166
```
The emergency key is generated during setup and appended to `/root/.ssh/authorized_keys`.

### 2. Emergency Helper Script
Inspect progress or rotate the key:
- Status: `./scripts/utils/ssh-emergency-access.sh -s`
- Rotate: `./scripts/utils/ssh-emergency-access.sh -r`
- Restore: `./scripts/utils/ssh-emergency-access.sh -b`

## Recovery Scenarios

### Scenario 1: Primary SSH key lost
1. Use the emergency key or helper script (`-e` or `-s`) to verify access.
2. Rotate the emergency key if it feels compromised:
   ```bash
   ./scripts/utils/ssh-emergency-access.sh -r
   ```
3. Replace the primary key in `/root/.ssh/authorized_keys` if needed.

### Scenario 2: SSH configuration broken
1. Connect via emergency key.
2. Restore the latest backup:
   ```bash
   ./scripts/utils/ssh-emergency-access.sh -b
   ```
3. Re-run the setup script if further changes are required.

### Scenario 3: Firewall blocks SSH
1. Use the emergency key with a provider console or a network that is already allowed.
2. Reset UFW from the emergency session:
   ```bash
   sudo ufw --force reset
   sudo ufw default deny incoming
   sudo ufw allow 22/tcp comment "SSH key-only access"
   sudo ufw allow 80/tcp comment "HTTP"
   sudo ufw allow 443/tcp comment "HTTPS"
   sudo ufw --force enable
   ```

## Preventing Future Lockouts
- Keep a copy of the emergency key offline (USB, safe deposit).
- Rotate the emergency key regularly with `./ssh-emergency-access.sh -r`.
- Document the backup path (`/root/backup-ssh-*`) and location of the restore script.
- After each recovery, re-run `/root/scripts/setup/setup-ssh-key-policy.sh` to ensure policy consistency.

## Verification
- Confirm `sshd -t` passes.
- Check `/etc/ssh/sshd_config.d/99-key-only.conf` still contains:
  ```bash
  PermitRootLogin prohibit-password
  PasswordAuthentication no
  AllowUsers root
  ```
- Review `/var/log/auth.log` for failed attempts.

## Troubleshooting Checklist
Before escalating:
1. Emergency key present? `ls /root/.ssh/emergency_key`
2. `authorized_keys` includes the right fingerprint.
3. Firewall allows SSH/HTTP/HTTPS.
4. `ssh-emergency-access.sh -s` shows policy health.
5. Logs show only the expected login methods.
