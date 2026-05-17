# SSH Key Best Practices

## Hardened Root Key Policy
- Use a strong ed25519 or RSA-4096 key for the primary `root` login.
- Never allow password authentication (`PasswordAuthentication no`).
- Keep `AllowUsers root` in `/etc/ssh/sshd_config.d/99-key-only.conf`.
- Regularly review `/var/log/auth.log` for unexpected authentication attempts.

## Emergency Key Management
1. The emergency key is stored at `/root/.ssh/emergency_key` and should be kept offline.
2. Rotate the emergency key quarterly with:
   ```bash
   ./scripts/utils/ssh-emergency-access.sh -r
   ```
3. After rotation, ensure the new `.pub` entry is the only emergency line in `/root/.ssh/authorized_keys`.

## Firewall Alignment
- `ufw` defaults should deny incoming, allow outgoing.
- Allow the following ports:
  ```bash
  ufw allow 22/tcp comment "SSH key-only access"
  ufw allow 80/tcp comment "HTTP"
  ufw allow 443/tcp comment "HTTPS"
  ```
- Monitor `ufw status verbose` after each deployment.

## Recovery Preparedness
- Keep a current backup directory under `/root/backup-ssh-*`.
- The restore helper script is located at `/root/backup-ssh-*/restore.sh`.
- Periodically verify the helper script works by running:
  ```bash
  ./scripts/utils/ssh-emergency-access.sh -b
  ```
  (This will prompt you before restoring.)

## Automation Recommendations
- Automate log checks via `scripts/diagnostics.sh`.
- Run `scripts/test/test-ssh-key-policy.sh` after any change to SSH-related scripts.
- Document any new authorized keys in your change log.

## Communication
- Share the location of the emergency helper script with trusted operators.
- Keep notes on how to restore the SSH policy in case of staff turnover.
