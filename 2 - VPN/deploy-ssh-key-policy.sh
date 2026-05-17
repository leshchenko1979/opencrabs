#!/bin/bash

# SSH Key Policy Deployment Script
# Deploys the key-only root SSH policy to the VDS server

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print colored output
print_status() {
    local status=$1
    local message=$2

    if [ "$status" == "OK" ]; then
        echo -e "${GREEN}[OK]${NC} $message"
    elif [ "$status" == "WARN" ]; then
        echo -e "${YELLOW}[WARN]${NC} $message"
    elif [ "$status" == "ERROR" ]; then
        echo -e "${RED}[ERROR]${NC} $message"
    else
        echo -e "[INFO] $message"
    fi
}

# Server configuration (use ~/.ssh/config Host "vpn" → 104.128.131.166)
SSH_HOST="${SSH_HOST:-vpn}"
SSH_USER="root"
SSH_KEY_PATH="${SSH_KEY_PATH:-$HOME/.ssh/id_rsa}"

# Function to check prerequisites
check_prerequisites() {
    print_status "INFO" "Checking prerequisites..."

    # Check SSH key
    if [ ! -f "$SSH_KEY_PATH" ]; then
        print_status "ERROR" "SSH key not found at $SSH_KEY_PATH"
        print_status "INFO" "Please ensure your SSH key exists and try again"
        exit 1
    fi

    # Check SSH connection
    if ! ssh -i "$SSH_KEY_PATH" -o ConnectTimeout=5 -o BatchMode=yes "$SSH_USER@$SSH_HOST" "echo 'Connection test'" 2>/dev/null; then
        print_status "ERROR" "Cannot connect to server with SSH key"
        print_status "INFO" "Please check your SSH key and network connection"
        exit 1
    fi

    print_status "OK" "Prerequisites check passed"
}

# Function to upload files
upload_files() {
    print_status "INFO" "Uploading files to server..."

    # Create temporary directory for uploads
    local temp_dir="/tmp/ssh-key-policy-upload-$(date +%s)"
    mkdir -p "$temp_dir"

    # Copy files to temporary directory
    cp -r scripts/ "$temp_dir/"
    cp -r docs/ "$temp_dir/"
    cp -r config/ "$temp_dir/"

    # Upload to server
    if scp -i "$SSH_KEY_PATH" -r "$temp_dir"/* "$SSH_USER@$SSH_HOST:/root/"; then
        print_status "OK" "Files uploaded successfully"
    else
        print_status "ERROR" "Failed to upload files"
        rm -rf "$temp_dir"
        exit 1
    fi

    # Clean up temporary directory
    rm -rf "$temp_dir"
}

# Function to run setup
run_setup() {
    print_status "INFO" "Running SSH key policy setup on server..."

    # Execute setup script on server
    ssh -i "$SSH_KEY_PATH" "$SSH_USER@$SSH_HOST" "
        set -e
        echo 'Starting SSH key policy setup...'

        # Make scripts executable
        chmod +x /root/scripts/setup/setup-ssh-key-policy.sh
        chmod +x /root/scripts/utils/ssh-emergency-access.sh

        # Run setup script
        sudo /root/scripts/setup/setup-ssh-key-policy.sh

        echo 'SSH key policy setup completed!'
    "

    if [ $? -eq 0 ]; then
        print_status "OK" "SSH key policy setup completed successfully"
    else
        print_status "ERROR" "SSH key policy setup failed"
        exit 1
    fi
}

# Function to display next steps
display_next_steps() {
    echo
    echo "=== Setup Complete! ==="
    echo
    echo "Next steps:"
    echo "1. Verify /etc/ssh/sshd_config.d/99-key-only.conf contains the policy"
    echo "2. Store the emergency key offline and keep it safe"
    echo "3. Confirm SSH/HTTP/HTTPS firewall rules are open"
    echo "4. Test emergency key access: ssh -i /root/.ssh/emergency_key root@104.128.131.166"
    echo "5. Review /var/log/auth.log for any unusual entries"
    echo
    echo "Useful commands:"
    echo "  ./scripts/connect.sh                    # Normal connection"
    echo "  ./scripts/connect.sh -e                # Emergency access"
    echo "  ./scripts/utils/ssh-emergency-access.sh -s  # Check status"
    echo
    echo "For detailed setup instructions, see: docs/ssh-key-setup.md"
}

# Function to handle errors
handle_error() {
    local exit_code=$?
    if [ $exit_code -ne 0 ]; then
        print_status "ERROR" "Deployment failed with exit code $exit_code"
        echo
        echo "Troubleshooting:"
        echo "1. Check your internet connection"
        echo "2. Verify SSH key permissions: chmod 600 $SSH_KEY_PATH"
        echo "3. Ensure server is accessible: ssh -i $SSH_KEY_PATH $SSH_USER@$SSH_HOST 'echo test'"
        echo "4. Check if you have sudo privileges on server"
    fi
    exit $exit_code
}

# Set error trap
trap handle_error ERR

# Main execution
main() {
    echo "SSH Key Policy Deployment Script"
    echo "==============================="
    echo
    echo "Server: $SSH_HOST"
    echo "User: $SSH_USER"
    echo "SSH Key: $SSH_KEY_PATH"
    echo

    # Check if user wants to continue
    read -p "This will deploy the key-only SSH policy to the server. Continue? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        print_status "INFO" "Deployment cancelled"
        exit 0
    fi

    # Execute deployment steps
    check_prerequisites
    upload_files
    run_setup
    display_next_steps
}

# Run main function
main "$@"