#!/bin/bash

# Deploy gatus and optionally assert health status
# Usage: deploy-report.sh [--assert=healthy|unhealthy]

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

REMOTE_USER="root"
REMOTE_HOST="${REMOTE_HOST:-104.128.131.166}"
SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST")"
SSH_KEY="${SSH_KEY:-/Users/leshchenko/.ssh/id_ed25519}"

# Parse --assert argument
ASSERT=""
for arg in "$@"; do
    case $arg in
        --assert=*)
            ASSERT="${arg#*=}"
            ;;
    esac
done

echo "=== Deploying Gatus ==="
bash "${SCRIPT_DIR}/deploy.sh"

echo "Waiting 15 seconds for check cycle..."
sleep 15

echo ""
echo "=== host-box2 status from docker logs ==="
LOG_RESULT=$(ssh -i "$SSH_KEY" "${REMOTE_USER}@${SSH_TARGET}" "docker logs gatus --since 20s 2>&1 | grep 'host-box2' | tail -1")
echo "$LOG_RESULT"

# Extract success value from logs
SUCCESS=$(echo "$LOG_RESULT" | grep -o 'success=[a-z]*' | cut -d= -f2)

if [ -z "$SUCCESS" ]; then
    echo "ERROR: Could not determine success from logs"
    exit 1
fi

if [ "$SUCCESS" = "true" ]; then
    HEALTH="healthy"
else
    HEALTH="unhealthy"
fi

echo ""
echo "Health: $HEALTH"

if [ -n "$ASSERT" ]; then
    echo ""
    echo "Asserting: $ASSERT"
    if [ "$HEALTH" = "$ASSERT" ]; then
        echo "PASS: Assertion '$ASSERT' matched"
        exit 0
    else
        echo "FAIL: Expected '$ASSERT' but got '$HEALTH'"
        exit 1
    fi
fi