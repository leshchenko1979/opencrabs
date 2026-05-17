#!/bin/bash
# Re-apply idempotent DOCKER-USER rules for published Postgres 5432 (n8n on operations_5).
# Docker bypasses UFW for published ports; UFW rule alone is not enough.
# Run after docker.service (see docker-postgres-n8n-firewall.service).

set -euo pipefail

N8N_IP="${N8N_IP:-144.31.98.200}"
PORT="${PORT:-5432}"

# Clear any previous instances of these rules (handles duplicates / reboot ordering).
while iptables -D DOCKER-USER -s "$N8N_IP" -p tcp -m tcp --dport "$PORT" -j RETURN 2>/dev/null; do :; done
while iptables -D DOCKER-USER -p tcp -m tcp --dport "$PORT" -j DROP 2>/dev/null; do :; done

iptables -I DOCKER-USER 1 -p tcp -m tcp --dport "$PORT" -j DROP
iptables -I DOCKER-USER 1 -s "$N8N_IP" -p tcp -m tcp --dport "$PORT" -j RETURN

while ip6tables -D DOCKER-USER -p tcp -m tcp --dport "$PORT" -j DROP 2>/dev/null; do :; done
ip6tables -I DOCKER-USER 1 -p tcp -m tcp --dport "$PORT" -j DROP
