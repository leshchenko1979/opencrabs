#!/bin/bash

set -euo pipefail

echo "Resetting firewall to standard policy (SSH key-only access)..."

ufw --force reset
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp comment "SSH key-only access"
ufw allow 80/tcp comment "HTTP"
ufw allow 443/tcp comment "HTTPS"
ufw --force enable

echo ""
echo "Firewall status:"
ufw status verbose

echo ""
echo "Firewall hardened for SSH key-only access."