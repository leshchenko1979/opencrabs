# n8n migration — legacy operations → operations_4 VDS

## Source (legacy)

- **Host**: `94.250.254.232` (`LEGACY_HOST` in `.env`; see [operations/.env](../../operations/.env)).
- **Compose**: `/root/services/n8n/docker-compose.yml`; data in Docker volume **`n8n_data`** mounted at `/home/node/` in the container.
- **Real path on disk**: run on legacy:

  ```bash
  docker volume inspect n8n_data -f '{{ .Mountpoint }}'
  ```

  Often under `/var/lib/docker/volumes/...` or Docker root on `/data` — **always inspect**.

- **Optional bind mount**: `/local-files` → `/files` in container. If workflows use filesystem binary data under `/files`, copy that directory to the new host (e.g. same path `/local-files` or adjust env).

- **Secrets not in git**: `docker inspect n8n --format '{{json .Config.Env}}'` (and any files under `/root/services/n8n/` on legacy). **`N8N_ENCRYPTION_KEY`** must match or credentials break.

## Destination (new)

- **Data**: `/var/lib/n8n` (owned by `n8n` user after bootstrap).
- **Env**: `/etc/n8n.env` — public URLs must match **`N8N_PRODUCTION_HOST`** in `.env` (e.g. `n8n.l1979.ru`) and your Caddy vhost.

## DNS

1. Point **`n8n.l1979.ru`** (and any other hostname you serve in Caddy) A → **new VDS IP** (`2.27.120.75`). Until cutover, you can leave production DNS on legacy and test via hosts file, SSH tunnel, or a temporary name — not required for this doc’s flow.
2. After the new instance is validated, **stop legacy n8n** permanently.

## Automated copy (from your Mac)

With `.env` filled and SSH keys to **both** hosts:

```bash
./scripts/migrate-n8n-from-legacy.sh
```

The script stops n8n on legacy, streams the volume directory with **tar over SSH** (rsync cannot use two remote paths), unpacks into `/var/lib/n8n` on the new host, and can start legacy again with `--resume-legacy`.

## Manual steps

1. **Stop** n8n on legacy (consistent SQLite):

   ```bash
   ssh root@LEGACY_HOST 'cd /root/services/n8n && docker compose stop'
   ```

2. **Copy** `Mountpoint` contents to new host `/var/lib/n8n/` (e.g. `tar` stream via SSH, or rsync to a local staging dir then rsync to the new host).

3. On new host: `chown -R n8n:n8n /var/lib/n8n`

4. Merge env into `/etc/n8n.env`; **`N8N_USER_FOLDER=/var/lib/n8n`**; **`N8N_HOST`**, **`WEBHOOK_URL`**, **`N8N_EDITOR_BASE_URL`** aligned with your production hostname.

5. `systemctl restart n8n caddy`

6. Open `https://n8n.l1979.ru` (or your production host) — login, check credentials/workflows, trigger a test webhook.

7. Cut over production DNS if not already; **stop** legacy n8n container permanently.

**Note:** Webhook URLs inside workflow nodes may still reference the old hostname until you edit them or set production URLs in env before reactivation.
