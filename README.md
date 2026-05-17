# VDS Servers — l1979.ru


| Box            | IP              | Region  | RAM   | Disk           | Services                                                                                             |
| -------------- | --------------- | ------- | ----- | -------------- | ---------------------------------------------------------------------------------------------------- |
| 2 - VPN        | 104.128.131.166 | Finland | 709Mi | 4GB SSD (57%)  | AmneziaWG (46657/UDP), Traefik (80/443/8080), Gatus, reverse SSH tunnel (port 4444)                  |
| 3 - apps       | 144.31.188.163  | Germany | 961Mi | 10GB SSD (79%) | Traefik, Sablier, Redis, PostgreSQL, Redevest CRM, pdf-extract, AI Antispam, Business Tinder, TG MCP |
| 4 - n8n + claw | 2.27.120.75     | Germany | 961Mi | 10GB SSD (76%) | n8n (5678), Picoclaw (18790/18809), Caddy                                                           |

## SSH from your Mac

Define short host names in `~/.ssh/config` (root + your usual VDS key, e.g. `~/.ssh/id_ed25519`):

- `ssh vpn` → box 2 (104.128.131.166)
- `ssh apps` → box 3 (144.31.188.163)
- `ssh claw` → box 4 (2.27.120.75)

See `CLAUDE.md` (SSH Access Pattern) for the exact block shape and alignment with per-server `.env`.

Repo scripts map these three IPs to the same names for SSH (see `scripts/ssh-vds-host.sh`); keep real IPs in `.env` for URLs and documentation.
