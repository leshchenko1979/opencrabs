#!/bin/bash
#
# One-line setup for VDS servers (l1979.ru)
#
# Usage:
#   curl -sSL https://<your-repo-url>/setup.sh | bash -s -- [server-name|auto]
#
# Examples:
#   curl -sSL https://<your-repo-url>/setup.sh | bash
#   curl -sSL https://<your-repo-url>/setup.sh | bash -s -- "3 - apps"
#   curl -sSL https://<your-repo-url>/setup.sh | bash -s -- auto
#
# Server names: "2 - VPN", "3 - apps", "4 - n8n + claw", "auto" (interactive)
#

set -euo pipefail

GITHUB_REPO="${GITHUB_REPO:-https://github.com/1jehuang/vds-servers.git}"
SSH_KEY_DEFAULT="${HOME}/.ssh/id_ed25519"
INSTALL_DIR="${HOME}/vds-servers"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()    { echo -e "${GREEN}[INFO]${NC} $1"; }
warn()    { echo -e "${YELLOW}[WARN]${NC} $1"; }
error()   { echo -e "${RED}[ERROR]${NC} $1" >&2; }
success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }

# Detect server by SSH config or IP
detect_server() {
    local ip="$1"
    case "$ip" in
        104.128.131.166) echo "2 - VPN" ;;
        144.31.188.163)  echo "3 - apps" ;;
        2.27.120.75)     echo "4 - n8n + claw" ;;
        *)               echo "" ;;
    esac
}

# Check if running on local Mac or remote VDS
detect_local_or_remote() {
    if [ -f "/proc/version" ] || grep -qi "debian\|ubuntu\|fedora" /etc/os-release 2>/dev/null; then
        if [ "$(id -u)" -eq 0 ]; then
            echo "vds"
        else
            error "Please run as root on VDS: sudo $0"
            exit 1
        fi
    else
        echo "mac"
    fi
}

# Install prerequisites on macOS
install_prereqs_mac() {
    info "Checking prerequisites on macOS..."

    if ! command -v brew &>/dev/null; then
        warn "Homebrew not found. Install from: https://brew.sh"
    fi

    if ! command -v ssh &>/dev/null; then
        warn "OpenSSH not found. Install via Homebrew: brew install openssh"
    fi

    # Check for SSH key
    if [ ! -f "${SSH_KEY_DEFAULT}" ]; then
        warn "SSH key not found at ${SSH_KEY_DEFAULT}"
        info "Generate with: ssh-keygen -t ed25519 -C 'your@email.com' -f ~/.ssh/id_ed25519"
        return 1
    fi

    success "macOS prerequisites OK"
}

# Configure SSH access to a server
configure_ssh() {
    local server_name="$1"
    local host_ip="$2"
    local ssh_key="$3"

    info "Configuring SSH access to ${server_name} (${host_ip})..."

    # Test connection
    if ! ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -i "$ssh_key" "root@${host_ip}" "echo 'Connection OK'" &>/dev/null; then
        error "Cannot connect to ${host_ip}. Check:"
        error "  1. Server IP is correct"
        error "  2. SSH key is deployed to the server"
        error "  3. Firewall allows SSH (port 22)"
        return 1
    fi

    # Add to SSH config
    local ssh_config_entry="
# VDS: ${server_name} (${host_ip})
Host ${server_name,,//-/}
    HostName ${host_ip}
    User root
    IdentityFile ${ssh_key}
    StrictHostKeyChecking accept-new
    ServerAliveInterval 60
"

    local ssh_config="${HOME}/.ssh/config"
    if ! grep -q "${host_ip}" "$ssh_config" 2>/dev/null; then
        echo "$ssh_config_entry" >> "$ssh_config"
        chmod 600 "$ssh_config"
        success "Added ${server_name} to ~/.ssh/config"
    else
        info "SSH config already contains ${host_ip}"
    fi
}

# Setup a VDS server (run on the VDS itself)
setup_vds() {
    local server_name="$1"

    info "Setting up ${server_name} on VDS..."

    # Update system
    info "Updating system packages..."
    apt-get update -qq
    apt-get install -y -qq curl git docker.io docker-compose

    # Enable and start Docker
    systemctl enable docker
    systemctl start docker

    # Install repo
    if [ -d "${INSTALL_DIR}" ]; then
        info "Repo already exists, pulling latest..."
        cd "${INSTALL_DIR}" && git pull
    else
        info "Cloning repo..."
        git clone "$GITHUB_REPO" "${INSTALL_DIR}"
    fi

    cd "${INSTALL_DIR}/${server_name}"

    # Check for .env.example
    if [ -f ".env.example" ]; then
        if [ ! -f ".env" ]; then
            cp .env.example .env
            warn "Created .env from template — edit with your actual values:"
            info "  nano ${INSTALL_DIR}/${server_name}/.env"
        fi
    fi

    success "${server_name} VDS setup complete!"
    info "Next steps:"
    info "  1. Edit .env with your credentials"
    info "  2. Run: cd ${INSTALL_DIR}/${server_name} && ./scripts/deploy-<service>.sh"
}

# Deploy from Mac to VDS
deploy_from_mac() {
    local server_name="$1"
    local host_ip="$2"
    local ssh_key="$3"

    info "Deploying ${server_name} from Mac to ${host_ip}..."

    if [ ! -d "${server_name}" ]; then
        error "Server directory not found: ${server_name}"
        error "Run this script from the repo root"
        exit 1
    fi

    cd "${server_name}"

    # Check .env
    if [ ! -f ".env" ]; then
        if [ -f ".env.example" ]; then
            cp .env.example .env
            warn "Created .env from template — edit before deploying!"
        else
            error ".env not found and no .env.example available"
            exit 1
        fi
    fi

    # Source .env
    # shellcheck source=/dev/null
    source .env
    REMOTE_HOST_IP="${REMOTE_HOST_IP:-${host_ip}}"
    REMOTE_USER="${REMOTE_USER:-root}"
    SSH_KEY="${SSH_KEY:-${ssh_key}}"

    info "Deploying to ${REMOTE_USER}@${REMOTE_HOST_IP}..."

    # Create remote dir
    ssh -i "$SSH_KEY" "${REMOTE_USER}@${REMOTE_HOST_IP}" "mkdir -p /opt/services 2>/dev/null || true"

    # Sync services
    rsync -avz --delete \
        -e "ssh -i ${SSH_KEY}" \
        --exclude='.env' \
        --exclude='*.log' \
        --exclude='data/' \
        services/ "${REMOTE_USER}@${REMOTE_HOST_IP}:/opt/services/"

    success "Deployed services to ${server_name}"
    info "Check status: ssh -i ${SSH_KEY} ${REMOTE_USER}@${REMOTE_HOST_IP} 'docker ps'"
}

# Interactive server selection
select_server_interactive() {
    echo "Select a server to setup:"
    echo "  1) 2 - VPN          (104.128.131.166)"
    echo "  2) 3 - apps        (144.31.188.163)"
    echo "  3) 4 - n8n + claw  (2.27.120.75)"
    echo ""
    read -p "Enter choice [1-3] (default: 1): " choice
    choice="${choice:-1}"

    case "$choice" in
        1) echo "2 - VPN" ;;
        2) echo "3 - apps" ;;
        3) echo "4 - n8n + claw" ;;
        *) echo "2 - VPN" ;;
    esac
}

# Main
main() {
    local target="${1:-auto}"
    local context
    context=$(detect_local_or_remote)

    echo ""
    echo "============================================"
    echo "  VDS Servers One-Line Setup (l1979.ru)"
    echo "============================================"
    echo ""

    if [ "$context" = "mac" ]; then
        info "Running on macOS (local machine)"

        # Detect or select server
        if [ "$target" = "auto" ]; then
            target=$(select_server_interactive)
        fi

        # Server IP mapping
        case "$target" in
            "2 - VPN")         host_ip="104.128.131.166" ;;
            "3 - apps")        host_ip="144.31.188.163" ;;
            "4 - n8n + claw")  host_ip="2.27.120.75" ;;
            *) error "Unknown server: $target"; exit 1 ;;
        esac

        install_prereqs_mac || true
        configure_ssh "$target" "$host_ip" "${SSH_KEY_DEFAULT}"

        echo ""
        echo "============================================"
        success "${target} SSH access configured!"
        echo "============================================"
        echo ""
        info "To deploy from Mac:"
        info "  cd '${target}'"
        info "  ./scripts/deploy-<service>.sh"
        echo ""

    else
        info "Running on VDS (${context})"

        if [ "$target" = "auto" ]; then
            # Auto-detect which server this is
            host_ip=$(hostname -I | awk '{print $1}')
            target=$(detect_server "$host_ip")
            if [ -z "$target" ]; then
                error "Cannot auto-detect server for IP: $host_ip"
                error "Please specify server manually: setup.sh '3 - apps'"
                exit 1
            fi
        fi

        setup_vds "$target"
    fi
}

main "$@"
