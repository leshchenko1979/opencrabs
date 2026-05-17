#!/bin/bash

# DNS check: verify n8n.l1979.ru points to VDS IP
# DNS managed at reg.ru - update manually via https://www.reg.ru/user/account/
# Usage: ./scripts/fix-dns.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

[ -f "${PROJECT_ROOT}/.env" ] && source "${PROJECT_ROOT}/.env"

REMOTE_HOST_IP="${REMOTE_HOST_IP:-94.250.254.232}"
RECORD_NAME="n8n.l1979.ru"

echo "Checking ${RECORD_NAME}..."
RESOLVED=$(dig +short "${RECORD_NAME}" | tail -1)

if [ -z "$RESOLVED" ]; then
    echo "ERROR: No A record found for ${RECORD_NAME}"
    echo ""
    echo "Update DNS at reg.ru:"
    echo "  1. https://www.reg.ru/user/account/ → Domains → l1979.ru"
    echo "  2. Add or edit A record for n8n → ${REMOTE_HOST_IP}"
    echo ""
    echo "See docs/dns-setup.md for details."
    exit 1
fi

if [ "$RESOLVED" = "$REMOTE_HOST_IP" ]; then
    echo "OK: ${RECORD_NAME} → ${RESOLVED}"
else
    echo "MISMATCH: ${RECORD_NAME} resolves to ${RESOLVED}, expected ${REMOTE_HOST_IP}"
    echo ""
    echo "Update DNS at reg.ru:"
    echo "  1. https://www.reg.ru/user/account/ → Domains → l1979.ru"
    echo "  2. Edit A record for n8n → ${REMOTE_HOST_IP}"
    echo ""
    echo "See docs/dns-setup.md for details."
    exit 1
fi
