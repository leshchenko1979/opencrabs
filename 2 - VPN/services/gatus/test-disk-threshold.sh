#!/bin/bash
set -e

# DEPRECATED: tests old disk-check + [BODY] < threshold flow.
# Production uses host-diag + [STATUS] == 0 — see services/gatus/scripts/host-diag and deploy-host-diag.sh.

# Test gatus disk monitoring using wrapper scripts
# Success: threshold below current usage makes unhealthy, above makes healthy

REMOTE_USER="root"
SSH_KEY="${SSH_KEY:-/Users/leshchenko/.ssh/id_ed25519}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"
BOX2_IP="${BOX2_IP:-104.128.131.166}"
BOX2_SSH="$(vds_ssh_connect_host "$BOX2_IP")"

GATUS_URL="https://gatus.l1979.ru"
CONFIG_FILE="${SCRIPT_DIR}/config/config.yaml"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $1"; }
log_pass() { echo -e "${GREEN}[PASS]${NC} $1"; }

# Deploy wrapper script to a box
deploy_wrapper_script() {
    local host="$1"
    local ssh_target="$2"
    log_info "Deploying disk-check wrapper to ${host} (${ssh_target})..."

    ssh -i "$SSH_KEY" "${REMOTE_USER}@${ssh_target}" "cat > /usr/local/bin/disk-check << 'ENDSCRIPT'
#!/bin/bash
THRESHOLD=\${1:-90}
USAGE=\$(df / | awk 'NR==2{print int(\$5)}')
if [ \$USAGE -ge \$THRESHOLD ]; then
    exit 1
fi
exit 0
ENDSCRIPT
chmod +x /usr/local/bin/disk-check"
}

# Extract current disk usage for box2
get_box2_disk_usage() {
    ssh -i "$SSH_KEY" "${REMOTE_USER}@${BOX2_SSH}" "df / | awk 'NR==2{print int(\$5)}'" 2>/dev/null
}

# Update threshold in gatus config for a specific endpoint
update_threshold() {
    local new_threshold="$1"
    log_info "Updating threshold to ${new_threshold}%..."

    # Update the disk-check command with new threshold
    python3 -c "
import re
with open('$CONFIG_FILE', 'r') as f:
    content = f.read()

# Replace /usr/local/bin/disk-check NNN
pattern = r'/usr/local/bin/disk-check [0-9]+'
replacement = r'/usr/local/bin/disk-check ${new_threshold}'
new_content = re.sub(pattern, replacement, content)
with open('$CONFIG_FILE', 'w') as f:
    f.write(new_content)
"
    log_info "Updated config.yaml with threshold ${new_threshold}%"
}

# Find and update the special threshold comment
update_threshold_comment() {
    local new_val="$1"
    python3 -c "
import re
with open('$CONFIG_FILE', 'r') as f:
    content = f.read()
if 'threshold=' in content:
    content = re.sub(r'threshold=\d+', 'threshold=${new_val}', content)
else:
    content = re.sub(r'(name: disk-box2)', r'\1\n    # threshold=${new_val}', content)
with open('$CONFIG_FILE', 'w') as f:
    f.write(content)
"
}

# ===== MAIN TEST =====

log_info "=== Gatus Disk Threshold Test ==="

# Step 1: Deploy wrapper scripts to all boxes
log_info "Deploying wrapper scripts..."
deploy_wrapper_script "box2" "$BOX2_SSH"

# Step 2: Check current disk usage on box2
CURRENT_DISK=$(get_box2_disk_usage)
if [ -z "$CURRENT_DISK" ]; then
    log_fail "Cannot get disk usage from box2"
    exit 1
fi
log_info "Current disk usage on box2: ${CURRENT_DISK}%"

# Step 3: Set threshold 10 points ABOVE current disk usage (should be healthy)
HIGH_THRESHOLD=$((CURRENT_DISK + 10))
log_info ""
log_info "=== STEP 1: Testing threshold ${HIGH_THRESHOLD}% (above current ${CURRENT_DISK}%) ==="
log_info "Expected: HEALTHY (${CURRENT_DISK} < ${HIGH_THRESHOLD})"
update_threshold "$HIGH_THRESHOLD"
update_threshold_comment "$HIGH_THRESHOLD"

# Deploy and check
log_info "Deploying gatus..."
if bash "${SCRIPT_DIR}/deploy-report.sh" --assert=healthy; then
    log_pass "STEP 1 PASSED"
else
    log_fail "STEP 1 FAILED"
    exit 1
fi

# Step 4: Set threshold 10 points BELOW current disk usage (should be unhealthy)
LOW_THRESHOLD=$((CURRENT_DISK - 10))
log_info ""
log_info "=== STEP 2: Testing threshold ${LOW_THRESHOLD}% (below current ${CURRENT_DISK}%) ==="
log_info "Expected: UNHEALTHY (${CURRENT_DISK} >= ${LOW_THRESHOLD})"
update_threshold "$LOW_THRESHOLD"
update_threshold_comment "$LOW_THRESHOLD"

# Deploy and check
log_info "Deploying gatus..."
if bash "${SCRIPT_DIR}/deploy-report.sh" --assert=unhealthy; then
    log_pass "STEP 2 PASSED"
    log_info ""
    log_info "=== TEST SUCCESSFUL ==="
    log_info "Disk monitoring works correctly:"
    log_info "  - Threshold ${HIGH_THRESHOLD}% (above ${CURRENT_DISK}%) -> healthy"
    log_info "  - Threshold ${LOW_THRESHOLD}% (below ${CURRENT_DISK}%) -> unhealthy"
    echo "<promise>TEST SUCCESSFUL</promise>"
    exit 0
else
    log_fail "STEP 2 FAILED"
    exit 1
fi