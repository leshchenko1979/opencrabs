# VDS deployment (operations_3)

Central repo for deploying and operating Dockerized services on a single VDS (**l1979.ru** ecosystem; CRM at **redevest-crm.ru**).

## Quick start

- Copy `.env.example` → `.env` and set `REMOTE_HOST_IP`, `REMOTE_USER`, `SSH_KEY`, and service secrets.
- Deploy per service using scripts under `scripts/` (commands listed in [docs/services.md](docs/services.md)).

## Service catalog

Authoritative list: **[docs/services.md](docs/services.md)** (configs, paths, URLs, memory notes).

## Documentation

| Doc | Contents |
|-----|----------|
| [docs/services.md](docs/services.md) | Service configs, deploy commands, networks |
| [docs/maintenance.md](docs/maintenance.md) | Cleanup, swap, SSH, diagnostics |

## Diagnostics

From repo root:

```bash
cd ../scripts && ./diagnostics-unified.sh   # All servers at once
cd ../scripts && ./cleanup-unified.sh       # Cleanup all servers
```

Uses `.env` for SSH; full output appended to **`logs/diagnostics.log`** (echoed to terminal). For console-only: `./diagnostics-unified.sh --no-log`.
