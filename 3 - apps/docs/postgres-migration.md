# PostgreSQL on ops3 — deploy and migration from operations VDS

## Deploy (empty cluster)

- Set `POSTGRES_USER`, `POSTGRES_PASSWORD`, and optional `POSTGRES_MAJOR` (default `16`) in [operations_3/.env](../.env) — match legacy during migration.
- From repo: `cd operations_3 && ./scripts/deploy-postgres.sh`

**Data directory (host)**: `/data/projects/postgres/data` → `/var/lib/postgresql/data` in the container.

**Image**: `postgres:<major>-alpine` (smallest official variant). Align **major** with the source server (`SHOW server_version;`).

## Network

- **Containers on `traefik-public`**: hostname **`postgres`**, port **5432** (e.g. ai-antispam `PG_HOST=postgres`, Redevest CRM on this VDS `DATABASE_URL` host **`postgres`**).
- **Host**: Compose publishes **`5432:5432`** so **n8n** on **operations_5** (`144.31.98.200`) can connect to `144.31.188.163:5432`. Docker bypasses UFW for that forward — use **DOCKER-USER** + **UFW** as below. Ad-hoc on the server: `docker exec -it postgres psql -h 127.0.0.1 -U <user>`.

### n8n on operations_5 — allow Postgres only from that host

After `./scripts/deploy-postgres.sh` (compose includes `ports: "5432:5432"`):

1. **UFW**: `ufw allow from 144.31.98.200 to any port 5432 proto tcp comment 'n8n-ops5-postgres'`
2. **DOCKER-USER** (required — Docker bypasses UFW for published ports): from this repo on your Mac, `./scripts/install-postgres-n8n-firewall.sh` (uses `.env` `REMOTE_HOST_IP`), or install manually on **ops3**:
   - `services/postgres/iptables-docker-n8n.sh` → `/usr/local/sbin/iptables-docker-n8n.sh` (mode `0755`)
   - `services/postgres/docker-postgres-n8n-firewall.service` → `/etc/systemd/system/docker-postgres-n8n-firewall.service`
   - `systemctl daemon-reload && systemctl enable --now docker-postgres-n8n-firewall.service`

   Do **not** put raw `-A DOCKER-USER` lines in `/etc/ufw/after.rules` — `ufw reload` will duplicate them each time. The script clears and re-applies the same rules.

**n8n credential**: Host `144.31.188.163`, port `5432`, user/password from `.env`, SSL off unless you add TLS in front.

### Postgres reachable from any IP (not n8n-only)

Compose sets **`listen_addresses=*`** and **`pg_hba.conf`** (SCRAM for remote TCP). From this repo on your Mac:

`./scripts/open-postgres-public-firewall.sh`

That disables **`docker-postgres-n8n-firewall`**, installs **`iptables-docker-postgres-clear.sh`** on the VDS, removes DOCKER-USER **5432** rules, replaces the n8n-only UFW rule with **`5432/tcp` ALLOW Anywhere** (v4+v6), and **`ufw reload`**.

Manual equivalent: `systemctl disable --now docker-postgres-n8n-firewall.service`, run **`iptables-docker-postgres-clear.sh`** (same **`N8N_IP`** as when the allowlist was installed), then **`ufw allow 5432/tcp`**.

**Risk**: public **5432** — use strong DB passwords; prefer VPN or SSH tunnel when possible.

### Close public port 5432 (post-migration)

If **n8n** still uses a remote connection, either migrate it (e.g. SSH tunnel + `127.0.0.1`) or keep **`5432:5432`** until n8n no longer needs it. Otherwise closing the port will break n8n workflows.

After every consumer uses **`postgres`** on Docker (no remote host `144.31.188.163:5432`):

1. **Redeploy** Postgres from this repo so compose has no host mapping: `./scripts/deploy-postgres.sh`. On the host: `systemctl disable --now docker-postgres-n8n-firewall.service` and remove `/usr/local/sbin/iptables-docker-n8n.sh` if you installed them for n8n; delete the UFW rule for `144.31.98.200` / 5432.
2. **UFW**: remove the legacy rule, e.g. `ufw status numbered` then `ufw delete <n>` for `5432` / `94.250.254.232`, or `ufw delete allow from 94.250.254.232 to any port 5432` if that exact rule exists.
3. **`/etc/ufw/after.rules` and `/etc/ufw/after6.rules`**: delete the blocks marked **`operations_3 postgres`** (DOCKER-USER / IPv6 drop for published 5432) if you added them during migration.
4. **`ufw reload`** (or ensure rules are applied as your distro expects).

Historical note: while **`5432:5432`** was published, Docker bypassed UFW for that forward; **DOCKER-USER** + **after.rules** restricted IPv4 to **94.250.254.232** and dropped IPv6 to 5432.

## Global dump (`pg_dumpall`) and restore

Performed from **ops3** (reachable to legacy `:5432`):

1. **Dump to host** (password from legacy `postgres` user):

   ```bash
   mkdir -p /data/projects/postgres
   docker run --rm --network host -e PGPASSWORD='<legacy_password>' \
     -v /data/projects/postgres:/backup postgres:16-alpine \
     sh -c "pg_dumpall -h 94.250.254.232 -U postgres --clean --if-exists -f /backup/pg_global.sql"
   ```

2. **Deploy** (or ensure container is up): `./scripts/deploy-postgres.sh` from Mac with filled `.env`.

3. **Restore** (expect harmless errors on `DROP` of `postgres` role; do not use `ON_ERROR_STOP` unless you pre-edit the dump):

   ```bash
   cat /data/projects/postgres/pg_global.sql | docker exec -i postgres psql -h 127.0.0.1 -U postgres
   ```

4. **Verify**:

   ```bash
   docker exec postgres psql -h 127.0.0.1 -U postgres -tAc "SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY 1;"
   ```

## Cutover checklist

- **ai-antispam** (`/data/projects/ai-antispam/.env`): `PG_HOST=postgres`, then `docker compose up -d` in that directory.
- **Redevest CRM** on **this VDS**: `DATABASE_URL` host **`postgres`** (not the public IP). Repo template: `deployed_projects/redevest-crm/.env.prod`.
- **Legacy host** (during migration only): `DATABASE_URL` pointed at **`144.31.188.163:5432`** until CRM moved; then follow [Close public port 5432](#close-public-port-5432-post-migration) above.

## Rollback

- Point `PG_HOST` back to `94.250.254.232` and CRM `DATABASE_URL` back to the legacy IP; restart services.
- Start legacy `postgresql` if it was stopped.

## Decommission legacy

After a stable period and backups: stop/disable host PostgreSQL on **94.250.254.232** only when nothing else uses it.
