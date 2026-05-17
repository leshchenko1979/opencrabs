#!/bin/bash
# Remove DOCKER-USER rules that restrict published Postgres 5432 (same -D loops as iptables-docker-n8n.sh, without re-adding).
# Run on the VDS after disabling docker-postgres-n8n-firewall.service.

set -euo pipefail

N8N_IP="${N8N_IP:-144.31.98.200}"
PORT="${PORT:-5432}"

while iptables -D DOCKER-USER -s "$N8N_IP" -p tcp -m tcp --dport "$PORT" -j RETURN 2>/dev/null; do :; done
while iptables -D DOCKER-USER -p tcp -m tcp --dport "$PORT" -j DROP 2>/dev/null; do :; done
while ip6tables -D DOCKER-USER -p tcp -m tcp --dport "$PORT" -j DROP 2>/dev/null; do :; done
